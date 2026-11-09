//! The CRUD and catalog commands of 006 (FR-006 to FR-008, FR-013, FR-016 to FR-018, FR-026,
//! FR-027) against an in-memory node, with the answers a `refrouter` gives (measured on 7.0, and
//! on 9.0.4 where they changed).

mod common;

use common::{Mem, doc, field};
use teseladb_wire::error::{CommandError, INTERRUPTED_AT_SHUTDOWN};
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn ids(n: std::ops::Range<i32>) -> Value {
    Value::Array(n.map(|i| d(&[("_id", Value::Int32(i)), ("a", Value::Int32(i % 3))])).collect())
}

fn insert(m: &mut Mem, coll: &str, docs: Value) -> Document {
    m.run("db", doc(&[("insert", s(coll)), ("documents", docs)]))
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("no error in {r:?}"),
    }
}

fn first_batch(r: &Document) -> Vec<Value> {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a.clone(),
        other => panic!("{other:?}"),
    }
}

fn cursor_id(r: &Document) -> i64 {
    match field(r, "cursor.id") {
        Value::Int64(n) => *n,
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_ordered_insert_stops_at_the_duplicate() {
    let mut m = Mem::new();
    let r = insert(
        &mut m,
        "c",
        Value::Array(vec![
            d(&[("_id", 1.into())]),
            d(&[("_id", 1.into())]),
            d(&[("_id", 2.into())]),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(1));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("E11000 duplicate key error collection: db.c index: _id_ dup key: { _id: 1 }")
    );
    assert_eq!(field(&r, "writeErrors.0.keyValue"), &d(&[("_id", 1.into())]));
    let unordered = doc(&[
        ("insert", s("c")),
        (
            "documents",
            Value::Array(vec![
                d(&[("_id", 3.into())]),
                d(&[("_id", 1.into())]),
                d(&[("_id", 4.into())]),
            ]),
        ),
        ("ordered", false.into()),
    ]);
    let r = m.run("db", unordered);
    assert_eq!(field(&r, "n"), &Value::Int32(2));
    assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(1));
    // An `_id` is generated first when missing.
    let r = insert(&mut m, "c", Value::Array(vec![d(&[("x", 1.into())])]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    let r = m.run("db", doc(&[("find", s("c")), ("filter", d(&[("x", 1.into())]))]));
    let got = first_batch(&r);
    let Value::Document(got) = &got[0] else { panic!() };
    assert!(matches!(got.iter().next(), Some(("_id", Value::ObjectId(_)))));
}

#[test]
fn a_find_with_batch_size_two_over_five_documents_takes_three_batches() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..5));
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 2.into())]));
    assert_eq!(first_batch(&r).len(), 2);
    let id = cursor_id(&r);
    assert_ne!(id, 0);
    let more = |m: &mut Mem| {
        m.run(
            "db",
            doc(&[("getMore", Value::Int64(id)), ("collection", s("c")), ("batchSize", 2.into())]),
        )
    };
    let r2 = more(&mut m);
    assert_eq!(field(&r2, "cursor.id"), &Value::Int64(id));
    let r3 = more(&mut m);
    assert_eq!(field(&r3, "cursor.id"), &Value::Int64(0));
    let Value::Array(last) = field(&r3, "cursor.nextBatch") else { panic!() };
    assert_eq!(last.len(), 1);
    // The cursor is gone.
    assert_eq!(code(&more(&mut m)), 43);
}

#[test]
fn a_batch_that_fills_up_leaves_the_cursor_open() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..4));
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 4.into())]));
    let id = cursor_id(&r);
    assert_ne!(id, 0);
    let r = m.run("db", doc(&[("getMore", Value::Int64(id)), ("collection", s("c"))]));
    assert_eq!(field(&r, "cursor.id"), &Value::Int64(0));
    // `limit` without `batchSize`: the first batch is short, so the cursor closes.
    let r = m.run("db", doc(&[("find", s("c")), ("limit", 2.into())]));
    assert_eq!((first_batch(&r).len(), cursor_id(&r)), (2, 0));
    // `batchSize: 0` opens a cursor with nothing yet.
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 0.into())]));
    assert!(first_batch(&r).is_empty());
    assert_ne!(cursor_id(&r), 0);
    let r = m
        .run("db", doc(&[("find", s("c")), ("singleBatch", true.into()), ("batchSize", 1.into())]));
    assert_eq!(cursor_id(&r), 0);
}

#[test]
fn find_filters_sorts_skips_limits_and_projects() {
    let mut m = Mem::new();
    let docs: Vec<Value> = (0..5)
        .map(|i| {
            d(&[
                ("_id", i.into()),
                ("a", (i % 3).into()),
                ("b", Value::Array(vec![i.into(), (10 - i).into()])),
            ])
        })
        .collect();
    insert(&mut m, "c", Value::Array(docs));
    let r = m.run(
        "db",
        doc(&[
            ("find", s("c")),
            ("filter", d(&[("a", d(&[("$gte", 1.into())]))])),
            ("sort", d(&[("b", (-1).into())])),
            ("projection", d(&[("a", 1.into())])),
            ("skip", 1.into()),
            ("limit", 3.into()),
            ("batchSize", 1.into()),
        ]),
    );
    // As measured: matches 1, 2, 4; by largest of `b` descending: 1 (9), 2 (8), 4 (6); skip 1.
    assert_eq!(first_batch(&r), vec![d(&[("_id", 2.into()), ("a", 2.into())])]);
    let id = cursor_id(&r);
    let r = m.run(
        "db",
        doc(&[("getMore", Value::Int64(id)), ("collection", s("c")), ("batchSize", 1.into())]),
    );
    assert_eq!(
        field(&r, "cursor.nextBatch"),
        &Value::Array(vec![d(&[("_id", 4.into()), ("a", 1.into())])])
    );
    // A missing collection gives an empty, closed cursor; a bad filter still fails.
    let r = m.run("db", doc(&[("find", s("nope"))]));
    assert_eq!((first_batch(&r).len(), cursor_id(&r)), (0, 0));
    let r = m.run("db", doc(&[("find", s("nope")), ("filter", d(&[("$bad", 1.into())]))]));
    assert_eq!(code(&r), 2);
    let r = m.run("db", doc(&[("find", s("c")), ("sort", d(&[("a", 2.into())]))]));
    assert_eq!(code(&r), 15975);
    let r = m.run("db", doc(&[("find", s("c")), ("skip", (-1).into())]));
    assert_eq!(code(&r), 2);
}

#[test]
fn get_more_and_kill_cursors_check_what_they_are_given() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..3));
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 1.into())]));
    let id = cursor_id(&r);
    let r = m.run("db", doc(&[("getMore", 1.into()), ("collection", s("c"))]));
    assert_eq!(code(&r), 14);
    let r = m.run("db", doc(&[("getMore", Value::Int64(id))]));
    assert_eq!(code(&r), 40414);
    // Another collection: refused, and the cursor stays open (`refrouter` 9.0.4).
    let r = m.run("db", doc(&[("getMore", Value::Int64(id)), ("collection", s("d"))]));
    assert_eq!(code(&r), 13);
    let r = m.run("db", doc(&[("getMore", Value::Int64(id)), ("collection", s("c"))]));
    assert_eq!(field(&r, "cursor.nextBatch.1._id"), &Value::Int32(2));
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 1.into())]));
    let id = cursor_id(&r);
    let r = m.run(
        "db",
        doc(&[
            ("killCursors", s("c")),
            ("cursors", Value::Array(vec![Value::Int64(id), Value::Int64(5)])),
        ]),
    );
    assert_eq!(field(&r, "cursorsKilled"), &Value::Array(vec![Value::Int64(id)]));
    assert_eq!(field(&r, "cursorsNotFound"), &Value::Array(vec![Value::Int64(5)]));
    let r = m.run("db", doc(&[("killCursors", s("c")), ("cursors", Value::Array(vec![1.into()]))]));
    assert_eq!(code(&r), 14);
    assert_eq!(code(&m.run("db", doc(&[("killCursors", s("c"))]))), 40414);
}

#[test]
fn updates_report_matched_modified_upserted_and_errors() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..3));
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            (
                "updates",
                Value::Array(vec![
                    d(&[
                        ("q", d(&[("_id", 1.into())])),
                        ("u", d(&[("$set", d(&[("z", 1.into())]))])),
                    ]),
                    d(&[
                        ("q", d(&[("_id", 9.into())])),
                        ("u", d(&[("$set", d(&[("z", 1.into())]))])),
                        ("upsert", true.into()),
                    ]),
                    d(&[
                        ("q", d(&[("_id", 1.into())])),
                        ("u", d(&[("$set", d(&[("_id", 5.into())]))])),
                    ]),
                ]),
            ),
        ]),
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    assert_eq!(field(&r, "n"), &Value::Int32(2));
    assert_eq!(field(&r, "upserted.0"), &d(&[("index", 1.into()), ("_id", 9.into())]));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(66));
    // A multi update over a filter that is not an `_id` equality.
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            (
                "updates",
                Value::Array(vec![d(&[
                    ("q", d(&[("a", d(&[("$lt", 3.into())]))])),
                    ("u", d(&[("$inc", d(&[("k", 1.into())]))])),
                    ("multi", true.into()),
                ])]),
            ),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(3));
    // Nothing changes: matched, not modified.
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            (
                "updates",
                Value::Array(vec![d(&[
                    ("q", d(&[("_id", 0.into())])),
                    ("u", d(&[("$set", d(&[("a", 0.into())]))])),
                ])]),
            ),
        ]),
    );
    assert_eq!((field(&r, "n"), field(&r, "nModified")), (&Value::Int32(1), &Value::Int32(0)));
    // A replacement cannot be multi; `u` must be a document or a pipeline.
    let bad = |u: Value, multi: bool| {
        doc(&[
            ("update", s("c")),
            ("updates", Value::Array(vec![d(&[("q", d(&[])), ("u", u), ("multi", multi.into())])])),
        ])
    };
    assert_eq!(
        field(&m.run("db", bad(d(&[("x", 1.into())]), true)), "writeErrors.0.code"),
        &Value::Int32(9)
    );
    assert_eq!(code(&m.run("db", bad(1.into(), false))), 9);
    // An update of a missing collection matches nothing.
    let r = m.run(
        "db",
        doc(&[
            ("update", s("none")),
            (
                "updates",
                Value::Array(vec![d(&[
                    ("q", d(&[])),
                    ("u", d(&[("$set", d(&[("a", 1.into())]))])),
                ])]),
            ),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(0));
    assert_eq!(
        code(&m.run("db", doc(&[("update", s("c")), ("updates", Value::Array(vec![]))]))),
        16
    );
}

fn dec(t: &str) -> Value {
    Value::Decimal128(t.parse().unwrap())
}

/// `skip`, `limit` and `batchSize` of `find`, `getMore` and `count` read as `refrouter` 9.0.4 reads
/// them: a double is cut toward zero and saturates, a decimal is rounded half to even, NaN is 0;
/// `count`'s `limit` must be a whole number.
#[test]
fn numeric_arguments_are_read_as_refdb_reads_them() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..5));
    let found = |m: &mut Mem, arg: &str, v: &Value| {
        first_batch(&m.run("db", doc(&[("find", s("c")), (arg, v.clone())]))).len()
    };
    for (v, left) in [
        (Value::Double(1.5), 4),
        (Value::Double(f64::NAN), 5),
        (Value::Double(f64::INFINITY), 0),
        (Value::Double(1e20), 0),
        (dec("2.5"), 3),
        (dec("3.5"), 1),
        (dec("0.5000001"), 4),
        (dec("2.5000000000000000000001"), 2),
        (dec("1E+1"), 0),
        (dec("NaN"), 5),
        (dec("-0"), 5),
        (dec("Infinity"), 0),
    ] {
        assert_eq!(found(&mut m, "skip", &v), left, "skip {v:?}");
    }
    for (v, taken) in [
        (Value::Double(2.5), 2),
        (dec("1.5"), 2),
        (Value::Double(f64::NAN), 5),
        (Value::Double(f64::INFINITY), 5),
    ] {
        assert_eq!(found(&mut m, "limit", &v), taken, "limit {v:?}");
    }
    let min = "-9223372036854775808";
    for (arg, v, actual) in [
        ("skip", Value::Double(f64::NEG_INFINITY), min),
        ("skip", Value::Double(-1e20), min),
        ("skip", dec("-2.5"), "-2"),
        ("skip", Value::Int32(-1), "-1"),
        ("limit", Value::Int32(-3), "-3"),
        ("limit", Value::Double(-1.5), "-1"),
        ("batchSize", dec("-Infinity"), min),
    ] {
        let r = m.run("db", doc(&[("find", s("c")), (arg, v.clone())]));
        assert_eq!(code(&r), 2, "{arg} {v:?}");
        let msg = format!("BSON field '{arg}' value must be >= 0, actual value '{actual}'");
        assert_eq!(field(&r, "errmsg"), &s(&msg));
    }
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 1.into())]));
    let more = doc(&[
        ("getMore", Value::Int64(cursor_id(&r))),
        ("collection", s("c")),
        ("batchSize", (-1).into()),
    ]);
    let r = m.run("db", more);
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'batchSize' value must be >= 0, actual value '-1'")
    );
    let count = |m: &mut Mem, arg: &str, v: &Value| {
        m.run("db", doc(&[("count", s("c")), (arg, v.clone())]))
    };
    for (v, n) in [(dec("2.00"), 2), (dec("-2.0"), 2), (Value::Int64(-3), 3), (dec("-0"), 5)] {
        assert_eq!(field(&count(&mut m, "limit", &v), "n"), &Value::Int32(n), "{v:?}");
    }
    for (v, n) in [(Value::Double(f64::NAN), 5), (Value::Double(1e20), 0), (dec("1.5"), 3)] {
        assert_eq!(field(&count(&mut m, "skip", &v), "n"), &Value::Int32(n), "{v:?}");
    }
    let r = count(&mut m, "skip", &Value::Double(-1e20));
    assert_eq!(
        field(&r, "errmsg"),
        &s(&format!("BSON field 'skip' value must be >= 0, actual value '{min}'"))
    );
    for (v, msg) in [
        (Value::Double(1.5), "Expected an integer: limit: 1.5"),
        (Value::Double(-2.5), "Expected an integer: limit: -2.5"),
        (Value::Double(f64::NAN), "Expected an integer, but found NaN in: limit: nan"),
        (Value::Double(1e20), "Cannot represent as a 64-bit integer: limit: 1e+20"),
        (Value::Double(f64::NEG_INFINITY), "Cannot represent as a 64-bit integer: limit: -inf"),
        (dec("1.5"), "Cannot represent as a 64-bit integer: limit: 1.5"),
        (dec("NaN"), "Cannot represent as a 64-bit integer: limit: NaN"),
        (dec("1E+30"), "Cannot represent as a 64-bit integer: limit: 1E+30"),
        (
            dec("9223372036854775808"),
            "Cannot represent as a 64-bit integer: limit: 9223372036854775808",
        ),
    ] {
        let r = count(&mut m, "limit", &v);
        assert_eq!(code(&r), 9, "{v:?}");
        assert_eq!(field(&r, "errmsg"), &s(msg));
    }
}

#[test]
fn deletes_take_one_or_all() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..5));
    let one = d(&[("q", d(&[("a", 1.into())])), ("limit", 1.into())]);
    let all = d(&[("q", d(&[])), ("limit", 0.into())]);
    let r = m.run("db", doc(&[("delete", s("c")), ("deletes", Value::Array(vec![one, all]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(5));
    assert!(m.stored(..).is_empty());
}

/// A statement the parser rejects fails the whole command before any delete runs, with the
/// `limit` written as a C++ stream writes a double (measured in `refrouter` 9.0.4).
#[test]
fn a_malformed_delete_statement_fails_the_command() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..3));
    let one = d(&[("q", d(&[("_id", 0.into())])), ("limit", 1.into())]);
    let nan_with_sign = f64::from_bits(0xfff8_0000_0000_0000);
    for (limit, got) in [
        (Value::Int32(2), "2"),
        (Value::Int64(-1), "-1"),
        (Value::Double(2.0), "2"),
        (Value::Double(0.5), "0.5"),
        (Value::Double(0.999_999_9), "1"),
        (Value::Double(1_234_567.0), "1.23457e+06"),
        (Value::Double(f64::NAN), "nan"),
        (Value::Double(nan_with_sign), "-nan"),
        (Value::Double(f64::NEG_INFINITY), "-inf"),
        (dec("2.00"), "2"),
        (dec("1E+30"), "1e+30"),
        (dec("NaN"), "nan"),
    ] {
        let stmt = d(&[("q", d(&[])), ("limit", limit.clone())]);
        let cmd = doc(&[("delete", s("c")), ("deletes", Value::Array(vec![one.clone(), stmt]))]);
        let r = m.run("db", cmd);
        assert_eq!(field(&r, "ok"), &Value::Double(0.0), "{limit:?}");
        assert_eq!(code(&r), 9);
        let msg = format!("The limit field in delete objects must be 0 or 1. Got {got}");
        assert_eq!(field(&r, "errmsg"), &s(&msg));
    }
    for (stmt, code_of, msg) in [
        (
            d(&[("limit", 1.into())]),
            40414,
            "BSON field 'delete.deletes.q' is missing but a required field",
        ),
        (
            d(&[("q", d(&[]))]),
            40414,
            "BSON field 'delete.deletes.limit' is missing but a required field",
        ),
        (
            d(&[("q", 1.into()), ("limit", 1.into())]),
            14,
            "BSON field 'delete.deletes.q' is the wrong type 'int', expected type 'object'",
        ),
        (
            d(&[("q", d(&[])), ("limit", 1.into()), ("zz", 1.into())]),
            40415,
            "BSON field 'delete.deletes.zz' is an unknown field.",
        ),
    ] {
        let cmd = doc(&[("delete", s("c")), ("deletes", Value::Array(vec![one.clone(), stmt]))]);
        let r = m.run("db", cmd);
        assert_eq!(code(&r), code_of);
        assert_eq!(field(&r, "errmsg"), &s(msg));
    }
    assert_eq!(m.stored(..).len(), 3, "nothing was deleted");
    let bad = d(&[("q", d(&[("$bad", 1.into())])), ("limit", 1.into())]);
    let cmd = doc(&[
        ("delete", s("c")),
        ("deletes", Value::Array(vec![bad, one.clone()])),
        ("ordered", false.into()),
    ]);
    let r = m.run("db", cmd);
    assert_eq!(
        (field(&r, "n"), field(&r, "writeErrors.0.code")),
        (&Value::Int32(1), &Value::Int32(2))
    );
    insert(&mut m, "c", ids(0..1));
    // A `limit` of 1 in any numeric type deletes one; any other type counts as 0.
    let delete = |m: &mut Mem, limit: &Value| {
        let stmt = d(&[("q", d(&[])), ("limit", limit.clone())]);
        m.run("db", doc(&[("delete", s("c")), ("deletes", Value::Array(vec![stmt]))]))
    };
    for limit in [Value::Double(1.0), dec("1.0")] {
        assert_eq!(field(&delete(&mut m, &limit), "n"), &Value::Int32(1), "{limit:?}");
    }
    for limit in [Value::Double(-0.0), Value::Boolean(true), s("x"), Value::Null] {
        insert(&mut m, "c", ids(10..12));
        let all = i32::try_from(m.stored(..).len()).unwrap();
        let r = delete(&mut m, &limit);
        assert_eq!(field(&r, "n"), &Value::Int32(all), "{limit:?}");
        assert!(m.stored(..).is_empty());
    }
}

#[test]
fn find_and_modify_returns_the_version_asked_for() {
    let mut m = Mem::new();
    let fam = |extra: &[(&str, Value)]| {
        let mut c = doc(&[("findAndModify", s("c")), ("query", d(&[("_id", 5.into())]))]);
        for (k, v) in extra {
            c.push(*k, v.clone());
        }
        c
    };
    let r = m.run(
        "db",
        fam(&[
            ("update", d(&[("$inc", d(&[("n", 1.into())]))])),
            ("new", true.into()),
            ("upsert", true.into()),
        ]),
    );
    assert_eq!(
        field(&r, "lastErrorObject"),
        &d(&[("n", 1.into()), ("updatedExisting", false.into()), ("upserted", 5.into())])
    );
    assert_eq!(field(&r, "value"), &d(&[("_id", 5.into()), ("n", 1.into())]));
    let r = m.run(
        "db",
        fam(&[
            ("update", d(&[("$inc", d(&[("n", 1.into())]))])),
            ("fields", d(&[("n", 1.into()), ("_id", 0.into())])),
        ]),
    );
    assert_eq!(field(&r, "value"), &d(&[("n", 1.into())]));
    let r = m.run("db", fam(&[("remove", true.into())]));
    assert_eq!(field(&r, "lastErrorObject"), &d(&[("n", 1.into())]));
    assert_eq!(field(&r, "value"), &d(&[("_id", 5.into()), ("n", 2.into())]));
    let r = m.run("db", fam(&[("remove", true.into())]));
    assert_eq!(field(&r, "value"), &Value::Null);
    assert_eq!(code(&m.run("db", fam(&[]))), 9);
    assert_eq!(code(&m.run("db", fam(&[("remove", true.into()), ("update", d(&[]))]))), 9);
    assert_eq!(code(&m.run("db", fam(&[("remove", true.into()), ("upsert", true.into())]))), 9);
    assert_eq!(code(&m.run("db", fam(&[("remove", true.into()), ("new", true.into())]))), 9);
    // With a sort, the first in that order.
    insert(&mut m, "c", ids(0..4));
    let r = m.run(
        "db",
        doc(&[
            ("findAndModify", s("c")),
            ("query", d(&[])),
            ("sort", d(&[("_id", (-1).into())])),
            ("remove", true.into()),
        ]),
    );
    assert_eq!(field(&r, "value._id"), &Value::Int32(3));
}

#[test]
fn count_and_distinct_read_what_matches() {
    let mut m = Mem::new();
    insert(
        &mut m,
        "c",
        Value::Array(vec![
            d(&[("_id", 1.into()), ("a", Value::Array(vec![1.into(), 2.into()]))]),
            d(&[("_id", 2.into()), ("a", 2.into())]),
        ]),
    );
    let r = m.run("db", doc(&[("count", s("c")), ("query", d(&[("a", 2.into())]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(2));
    let r = m.run("db", doc(&[("count", s("c")), ("skip", 1.into()), ("limit", 5.into())]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    let r = m.run("db", doc(&[("count", s("nope"))]));
    assert_eq!(field(&r, "n"), &Value::Int32(0));
    let r = m.run("db", doc(&[("distinct", s("c")), ("key", s("a"))]));
    assert_eq!(field(&r, "values"), &Value::Array(vec![1.into(), 2.into()]));
    let r = m.run("db", doc(&[("distinct", s("nope")), ("key", s("a"))]));
    assert_eq!(field(&r, "values"), &Value::Array(vec![]));
    assert_eq!(code(&m.run("db", doc(&[("distinct", s("c"))]))), 40414);
    assert_eq!(code(&m.run("db", doc(&[("distinct", s("c")), ("key", 1.into())]))), 14);
    // An empty key is the field named "" (measured in `refrouter` 9.0.4: `[5]`).
    m.run(
        "db",
        doc(&[
            ("insert", s("e")),
            ("documents", Value::Array(vec![d(&[("", 5.into())]), d(&[("a", 1.into())])])),
        ]),
    );
    for (key, values) in
        [("", vec![Value::Int32(5)]), ("a.", vec![]), (".a", vec![]), ("a..b", vec![])]
    {
        let r = m.run("db", doc(&[("distinct", s("e")), ("key", s(key))]));
        assert_eq!(field(&r, "values"), &Value::Array(values), "{key:?}");
    }
}

#[test]
fn names_follow_refdb_rules() {
    let mut m = Mem::new();
    let r = m.run("a.b", doc(&[("insert", s("c")), ("documents", ids(0..1))]));
    assert_eq!(field(&r, "errmsg"), &s("'.' is an invalid character in a db name: a.b"));
    let r = m.run(&"x".repeat(64), doc(&[("insert", s("c")), ("documents", ids(0..1))]));
    assert_eq!(field(&r, "errmsg"), &s("db name must be at most 63 characters, found: 64"));
    let r = m.run("a b", doc(&[("insert", s("c")), ("documents", ids(0..1))]));
    assert_eq!(field(&r, "errmsg"), &s("Invalid namespace specified: a b.c"));
    let r = m.run("a$b", doc(&[("insert", s("c")), ("documents", ids(0..1))]));
    assert_eq!(
        field(&r, "errmsg"),
        &s("Database a$b could not be created :: caused by :: Invalid db name specified: a$b")
    );
    for (coll, msg) in [
        ("a$b", "Invalid collection name: a$b"),
        ("system.x", "Invalid system namespace: db.system.x"),
        ("a.system.b", "Invalid namespace: db.a.system.b"),
    ] {
        let r = insert(&mut m, coll, ids(0..2));
        assert_eq!(field(&r, "writeErrors.0.errmsg"), &s(msg), "{coll}");
    }
    let long = "x".repeat(300);
    let r = m.run(
        "db",
        doc(&[("insert", s(&long)), ("documents", ids(0..2)), ("ordered", false.into())]),
    );
    assert_eq!(field(&r, "writeErrors.1.code"), &Value::Int32(73));
    let r = insert(&mut m, "", ids(0..1));
    assert_eq!(field(&r, "errmsg"), &s("Invalid namespace specified: db"));
    let r = m.run("db", doc(&[("insert", 1.into()), ("documents", ids(0..1))]));
    assert_eq!(code(&r), 73);
}

#[test]
fn the_catalog_commands_answer_as_refrouter() {
    let mut m = Mem::new();
    assert_eq!(field(&m.run("db", doc(&[("create", s("c"))])), "ok"), &Value::Double(1.0));
    // The same options again: fine; others: `NamespaceExists`.
    assert_eq!(field(&m.run("db", doc(&[("create", s("c"))])), "ok"), &Value::Double(1.0));
    let r = m.run("db", doc(&[("create", s("c")), ("validator", d(&[("x", 1.into())]))]));
    assert_eq!(code(&r), 48);
    let validator = d(&[("a", d(&[("$exists", true.into())]))]);
    m.run("db", doc(&[("create", s("v")), ("validator", validator.clone())]));
    assert_eq!(code(&m.run("db", doc(&[("create", s("w")), ("validationLevel", s("bogus"))]))), 2);
    assert_eq!(code(&m.run("db", doc(&[("create", s("w")), ("validator", 1.into())]))), 14);
    let r = m.run("db", doc(&[("listCollections", 1.into())]));
    let batch = first_batch(&r);
    assert_eq!(batch.len(), 2);
    assert_eq!(field(&r, "cursor.ns"), &s("db.$cmd.listCollections"));
    let Value::Document(v) = &batch[1] else { panic!() };
    assert_eq!(field(v, "options.validator"), &validator);
    assert_eq!(field(v, "idIndex.name"), &s("_id_"));
    let r = m.run(
        "db",
        doc(&[
            ("listCollections", 1.into()),
            ("nameOnly", true.into()),
            ("filter", d(&[("name", s("v"))])),
        ]),
    );
    assert_eq!(first_batch(&r), vec![d(&[("name", s("v")), ("type", s("collection"))])]);
    // Validation applies on insert.
    let r = insert(&mut m, "v", Value::Array(vec![d(&[("_id", 1.into())])]));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(121));
    assert!(matches!(field(&r, "writeErrors.0.errInfo"), Value::Document(_)));
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("v")),
            ("documents", Value::Array(vec![d(&[("_id", 1.into())])])),
            ("bypassDocumentValidation", true.into()),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    // collMod.
    assert_eq!(
        field(
            &m.run("db", doc(&[("collMod", s("v")), ("validator", d(&[("b", 1.into())]))])),
            "ok"
        ),
        &Value::Double(1.0)
    );
    assert_eq!(
        field(&m.run("db", doc(&[("collMod", s("nope")), ("validator", d(&[]))])), "errmsg"),
        &s("namespace does not exist: db.nope")
    );
    assert_eq!(code(&m.run("db", doc(&[("collMod", s("v")), ("capped", true.into())]))), 40415);
    // renameCollection.
    let rename = |from: &str, to: &str, drop: bool| {
        doc(&[("renameCollection", s(from)), ("to", s(to)), ("dropTarget", drop.into())])
    };
    let r = m.run("admin", rename("db.c", "db.v", false));
    assert_eq!(field(&r, "errmsg"), &s("Target collection db.v exists but dropTarget is not set"));
    assert_eq!(
        field(&m.run("admin", rename("db.zz", "db.e", false)), "errmsg"),
        &s("Collection db.zz doesn't exist.")
    );
    assert_eq!(code(&m.run("admin", rename("db.c", "db.c", false))), 20);
    assert_eq!(code(&m.run("db", rename("db.c", "db.e", false))), 13);
    assert_eq!(code(&m.run("admin", rename("nodot", "db.e", false))), 73);
    assert_eq!(field(&m.run("admin", rename("db.c", "db.v", true)), "ok"), &Value::Double(1.0));
    // The dropped target's documents are deleted.
    assert!(m.stored(..).is_empty());
    assert!(m.catalog.dropped.is_empty());
    let r = m.run("admin", doc(&[("listDatabases", 1.into()), ("nameOnly", true.into())]));
    let names = ["admin", "config", "db"].map(|n| d(&[("name", s(n))]));
    assert_eq!(field(&r, "databases"), &Value::Array(names.to_vec()));
    // drop and dropDatabase, also of what does not exist.
    insert(&mut m, "x", ids(0..3));
    assert_eq!(field(&m.run("db", doc(&[("drop", s("x"))])), "ok"), &Value::Double(1.0));
    assert_eq!(field(&m.run("db", doc(&[("drop", s("x"))])), "ok"), &Value::Double(1.0));
    assert!(m.stored(..).is_empty());
    insert(&mut m, "y", ids(0..3));
    assert_eq!(field(&m.run("db", doc(&[("dropDatabase", 1.into())])), "ok"), &Value::Double(1.0));
    assert!(m.catalog.collections.is_empty() && m.stored(..).is_empty());
    assert_eq!(field(&m.run("db", doc(&[("dropDatabase", 1.into())])), "ok"), &Value::Double(1.0));
}

#[test]
fn retryable_writes_run_once() {
    let mut m = Mem::new();
    let lsid = d(&[("id", Value::Binary { subtype: 4, bytes: vec![7; 16] })]);
    let cmd = doc(&[
        ("insert", s("c")),
        ("documents", ids(0..2)),
        ("lsid", lsid.clone()),
        ("txnNumber", Value::Int64(3)),
    ]);
    let first = m.run("db", cmd.clone());
    let writes = m.writes;
    let again = m.run("db", cmd);
    // The same answer, which lists the statements answered from their records (measured on a
    // `refrouter` 9.0.4).
    let retried = Value::Array(vec![Value::Int32(0), Value::Int32(1)]);
    assert_eq!(first, doc(&[("n", Value::Int32(2)), ("ok", 1.0.into())]));
    assert_eq!(
        again,
        doc(&[("n", Value::Int32(2)), ("retriedStmtIds", retried), ("ok", 1.0.into())])
    );
    assert_eq!(m.writes, writes);
    let old = doc(&[
        ("insert", s("c")),
        ("documents", ids(5..6)),
        ("lsid", lsid),
        ("txnNumber", Value::Int64(2)),
    ]);
    assert_eq!(code(&m.run("db", old)), 225);
}

#[test]
fn write_concern_errors_come_with_the_write() {
    let mut m = Mem::new();
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            ("documents", ids(0..1)),
            ("writeConcern", d(&[("w", 5.into())])),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    assert_eq!(field(&r, "writeConcernError.code"), &Value::Int32(100));
    assert_eq!(
        field(&r, "writeConcernError.errmsg"),
        &s(
            "UnsatisfiableWriteConcern: Not enough data-bearing nodes; Error details: { writeConcern: { w: 5, wtimeout: 0, provenance: \"clientSupplied\" } } at node1"
        )
    );
    // An unknown tag writes and reports it, with the concern as given (measured in 9.0.4).
    let wc = d(&[("w", s("tagged")), ("j", true.into()), ("wtimeout", 7.into())]);
    let r =
        m.run("db", doc(&[("insert", s("c")), ("documents", ids(30..31)), ("writeConcern", wc)]));
    assert_eq!((field(&r, "n"), field(&r, "ok")), (&Value::Int32(1), &Value::Double(1.0)));
    assert_eq!(field(&r, "writeConcernError.code"), &Value::Int32(79));
    assert_eq!(
        field(&r, "writeConcernError.errmsg"),
        &s(
            "UnknownReplWriteConcern: No write concern mode named 'tagged' found in replica set configuration; Error details: { writeConcern: { w: \"tagged\", j: true, wtimeout: 7, provenance: \"clientSupplied\" } } at node1"
        )
    );
    assert_eq!(field(&r, "writeConcernError.errInfo"), &d(&[]));
    // `w: 0` writes and answers `{ok: 1}` alone, even for a duplicate.
    let zero = || d(&[("w", 0.into())]);
    let r = m.run(
        "db",
        doc(&[("insert", s("c")), ("documents", ids(40..41)), ("writeConcern", zero())]),
    );
    assert_eq!(r, doc(&[("ok", Value::Double(1.0))]));
    let r = m.run(
        "db",
        doc(&[("insert", s("c")), ("documents", ids(40..41)), ("writeConcern", zero())]),
    );
    assert_eq!(r, doc(&[("ok", Value::Double(1.0))]));
    let r = m.run("db", doc(&[("count", s("c")), ("query", d(&[("_id", 40.into())]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    // `j` with `w: 0` writes too, and a fractional `w` is truncated (measured in 9.0.4).
    let wc = d(&[("w", 0.into()), ("j", true.into())]);
    let r =
        m.run("db", doc(&[("insert", s("c")), ("documents", ids(41..42)), ("writeConcern", wc)]));
    assert_eq!(r, doc(&[("ok", Value::Double(1.0))]));
    let wc = d(&[("w", Value::Double(1.5))]);
    let r =
        m.run("db", doc(&[("insert", s("c")), ("documents", ids(42..43)), ("writeConcern", wc)]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    for (wc, code_) in [
        (d(&[("w", true.into())]), 9),
        (d(&[("w", (-1).into())]), 9),
        (d(&[("w", d(&[]))]), 9),
        (d(&[("j", true.into()), ("fsync", true.into())]), 9),
        (d(&[("j", s("x"))]), 14),
        (d(&[("w", 1.into()), ("foo", 1.into())]), 40415),
        (d(&[("w", 1.into()), ("provenance", s("implicitDefault"))]), 72),
    ] {
        let r =
            m.run("db", doc(&[("insert", s("c")), ("documents", ids(1..2)), ("writeConcern", wc)]));
        assert_eq!(code(&r), code_);
    }
    // A failed write is reported for its documents.
    m.fail_next = Some(CommandError::new(INTERRUPTED_AT_SHUTDOWN, "stop"));
    let r = m.run(
        "db",
        doc(&[("insert", s("c")), ("documents", ids(10..12)), ("ordered", false.into())]),
    );
    assert_eq!(field(&r, "writeErrors.1.code"), &Value::Int32(11600));
}

#[test]
fn sessions_end_with_their_cursors() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..3));
    let r = m.run("admin", doc(&[("startSession", 1.into())]));
    assert_eq!(field(&r, "timeoutMinutes"), &Value::Int32(30));
    let lsid = field(&r, "id").clone();
    let find = doc(&[("find", s("c")), ("batchSize", 1.into()), ("lsid", lsid.clone())]);
    let id = cursor_id(&m.run("db", find));
    assert_eq!(
        field(&m.run("admin", doc(&[("refreshSessions", Value::Array(vec![lsid.clone()]))])), "ok"),
        &Value::Double(1.0)
    );
    // Without the session the cursor is not its: 50800 on a `refrouter` 9.0.4 without users
    // (`validateLSID` in `cluster_find.cpp`, measured on the native `refrouter` on 09-11-2026).
    let r = m.run("db", doc(&[("getMore", Value::Int64(id)), ("collection", s("c"))]));
    assert_eq!(code(&r), 50800);
    // `endSessions` leaves the cursor until the next refresh of the sessions' record, as
    // `refrouter` 9.0.4 does; the refresh kills it.
    m.run("admin", doc(&[("endSessions", Value::Array(vec![lsid.clone()]))]));
    let more = || {
        doc(&[
            ("getMore", Value::Int64(id)),
            ("collection", s("c")),
            ("lsid", lsid.clone()),
            ("batchSize", 1.into()),
        ])
    };
    assert_eq!(field(&m.run("db", more()), "ok"), &Value::Double(1.0));
    let shared = std::mem::take(&mut m.shared);
    let now = m.now;
    teseladb_wire::commands::registry::refresh(&mut m, &shared, 0, now).unwrap();
    m.shared = shared;
    assert_eq!(code(&m.run("db", more())), 43);
    let id = cursor_id(
        &m.run("db", doc(&[("find", s("c")), ("batchSize", 1.into()), ("lsid", lsid.clone())])),
    );
    m.run("admin", doc(&[("killAllSessions", Value::Array(vec![]))]));
    let r = m.run(
        "db",
        doc(&[("getMore", Value::Int64(id)), ("collection", s("c")), ("lsid", lsid.clone())]),
    );
    assert_eq!(code(&r), 43);
    m.run("admin", doc(&[("killSessions", Value::Array(vec![lsid]))]));
    assert_eq!(code(&m.run("admin", doc(&[("endSessions", 1.into())]))), 14);
    assert_eq!(code(&m.run("admin", doc(&[("endSessions", Value::Array(vec![1.into()]))]))), 14);
    assert_eq!(code(&m.run("admin", doc(&[("endSessions", Value::Array(vec![d(&[])]))]))), 40414);
    assert_eq!(
        code(&m.run("admin", doc(&[("endSessions", Value::Array(vec![d(&[("id", 1.into())])]))]))),
        14
    );
    // The messages of a `refrouter` 9.0.4 (measured).
    let id1 = || d(&[("id", 1.into())]);
    for (cmd, arg, code_of, msg) in [
        (
            "endSessions",
            Value::Array(vec![d(&[])]),
            40414,
            "BSON field 'endSessions.endSessionsFromClient.id' is missing but a required field",
        ),
        (
            "endSessions",
            Value::Array(vec![id1()]),
            14,
            "BSON field 'endSessions.endSessionsFromClient.id' is the wrong type 'int', expected type 'binData'",
        ),
        (
            "endSessions",
            Value::Array(vec![1.into()]),
            14,
            "BSON field 'endSessions.endSessionsFromClient.0' is the wrong type 'int', expected type 'object'",
        ),
        (
            "endSessions",
            1.into(),
            14,
            "BSON field 'endSessions.endSessions' is the wrong type 'int', expected type 'array'",
        ),
        (
            "refreshSessions",
            Value::Array(vec![d(&[])]),
            40414,
            "BSON field 'refreshSessions.refreshSessionsFromClient.id' is missing but a required field",
        ),
        (
            "killSessions",
            Value::Array(vec![id1()]),
            14,
            "BSON field 'KillSessionsCmd.killSessions.id' is the wrong type 'int', expected type 'binData'",
        ),
        (
            "killSessions",
            1.into(),
            14,
            "BSON field 'KillSessionsCmd.killSessions' is the wrong type 'int', expected type 'array'",
        ),
        (
            "killSessions",
            Value::Array(vec![1.into()]),
            14,
            "BSON field 'KillSessionsCmd.killSessions.0' is the wrong type 'int', expected type 'object'",
        ),
        (
            "killAllSessions",
            1.into(),
            14,
            "BSON field 'KillAllSessionsCmd.killAllSessions' is the wrong type 'int', expected type 'array'",
        ),
        (
            "killAllSessions",
            Value::Array(vec![1.into()]),
            14,
            "BSON field 'KillAllSessionsCmd.killAllSessions.0' is the wrong type 'int', expected type 'object'",
        ),
        (
            "killAllSessions",
            Value::Array(vec![d(&[])]),
            40414,
            "BSON field 'KillAllSessionsCmd.killAllSessions.db' is missing but a required field",
        ),
        (
            "killAllSessions",
            Value::Array(vec![d(&[("db", s("x"))])]),
            40414,
            "BSON field 'KillAllSessionsCmd.killAllSessions.user' is missing but a required field",
        ),
        (
            "killAllSessions",
            Value::Array(vec![d(&[("user", 1.into()), ("db", s("y"))])]),
            14,
            "BSON field 'KillAllSessionsCmd.killAllSessions.user' is the wrong type 'int', expected type 'string'",
        ),
    ] {
        let r = m.run("admin", doc(&[(cmd, arg)]));
        assert_eq!((code(&r), field(&r, "errmsg")), (code_of, &s(msg)));
    }
    // Without users, a list of users reaches no session: the cursor lives on.
    let id = cursor_id(&m.run("db", doc(&[("find", s("c")), ("batchSize", 1.into())])));
    let users = Value::Array(vec![d(&[("user", s("x")), ("db", s("y"))])]);
    assert_eq!(
        field(&m.run("admin", doc(&[("killAllSessions", users)])), "ok"),
        &Value::Double(1.0)
    );
    let r = m.run("db", doc(&[("getMore", Value::Int64(id)), ("collection", s("c"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
}

#[test]
fn bulk_write_reports_each_operation() {
    let mut m = Mem::new();
    let ns_info = Value::Array(vec![d(&[("ns", s("db.c"))])]);
    let ops = Value::Array(vec![
        d(&[("insert", 0.into()), ("document", d(&[("_id", 1.into())]))]),
        d(&[("insert", 0.into()), ("document", d(&[("_id", 1.into())]))]),
        d(&[
            ("update", 0.into()),
            ("filter", d(&[("_id", 2.into())])),
            ("updateMods", d(&[("$set", d(&[("a", 1.into())]))])),
            ("upsert", true.into()),
        ]),
        d(&[("delete", 0.into()), ("filter", d(&[("_id", 1.into())]))]),
        d(&[("insert", 0.5.into()), ("document", d(&[("_id", 3.into())]))]),
    ]);
    let bulk = |ops: Value| {
        doc(&[
            ("bulkWrite", 1.into()),
            ("ops", ops),
            ("nsInfo", ns_info.clone()),
            ("ordered", false.into()),
        ])
    };
    let r = m.run("admin", bulk(ops));
    assert_eq!(field(&r, "nInserted"), &Value::Int32(2));
    assert_eq!(field(&r, "nUpserted"), &Value::Int32(1));
    assert_eq!(field(&r, "nDeleted"), &Value::Int32(1));
    assert_eq!(field(&r, "nErrors"), &Value::Int32(1));
    assert_eq!(field(&r, "cursor.firstBatch.1.code"), &Value::Int32(11000));
    assert_eq!(field(&r, "cursor.firstBatch.2.upserted"), &d(&[("_id", 2.into())]));
    assert_eq!(
        code(&m.run("admin", doc(&[("bulkWrite", 1.into()), ("ops", Value::Array(vec![]))]))),
        40414
    );
    // What the parser refuses fails the whole command, even unordered (measured in 9.0.4).
    let good = d(&[("insert", 0.into()), ("document", d(&[("_id", 9.into())]))]);
    let doc_with_id = d(&[("_id", 1.into()), ("a", Value::Array(vec![1.into(), s("x")]))]);
    for (bad, code_of, msg) in [
        (
            5.into(),
            14,
            "BSON field 'bulkWrite.ops.1' is the wrong type 'int', expected type 'object'",
        ),
        (d(&[("bogus", 0.into())]), 40415, "BSON field 'bulkWrite.bogus' is an unknown field."),
        (d(&[]), 40415, "BSON field 'bulkWrite' is an unknown field."),
        (
            d(&[("insert", 0.into()), ("document", d(&[])), ("zz", 1.into())]),
            40415,
            "BSON field 'bulkWrite.ops.zz' is an unknown field.",
        ),
        (
            d(&[("insert", s("x")), ("document", d(&[]))]),
            14,
            "BSON field 'bulkWrite.ops.insert' is the wrong type 'string', expected types '[int, double, long, decimal]'",
        ),
        (
            d(&[("insert", (-1).into()), ("document", d(&[]))]),
            2,
            "BSON field 'insert' value must be >= 0, actual value '-1'",
        ),
        (
            d(&[("insert", 5.into()), ("document", doc_with_id)]),
            2,
            r#"BulkWrite ops entry { insert: 5, document: { _id: 1, a: [ 1, "x" ] } } has an invalid nsInfo index."#,
        ),
        (
            d(&[("delete", 0.into())]),
            40414,
            "BSON field 'bulkWrite.ops.filter' is missing but a required field",
        ),
    ] {
        let r = m.run("admin", bulk(Value::Array(vec![good.clone(), bad])));
        assert_eq!((code(&r), field(&r, "errmsg")), (code_of, &s(msg)));
    }
    let r = m.run("db", doc(&[("count", s("c")), ("query", d(&[("_id", 9.into())]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(0), "nothing ran");
    // Ordered, the first failure stops the rest; `bypassDocumentValidation` reaches updates.
    let dup = d(&[("insert", 0.into()), ("document", d(&[("_id", 2.into())]))]);
    let set = d(&[
        ("update", 0.into()),
        ("filter", d(&[("_id", 2.into())])),
        ("updateMods", d(&[("$set", d(&[("b", 1.into())]))])),
    ]);
    let cmd = doc(&[
        ("bulkWrite", 1.into()),
        ("ops", Value::Array(vec![dup, set.clone()])),
        ("nsInfo", ns_info.clone()),
    ]);
    let r = m.run("admin", cmd);
    assert_eq!((field(&r, "nErrors"), field(&r, "nMatched")), (&Value::Int32(1), &Value::Int32(0)));
    let cmd = doc(&[
        ("bulkWrite", 1.into()),
        ("ops", Value::Array(vec![set])),
        ("nsInfo", ns_info),
        ("bypassDocumentValidation", true.into()),
    ]);
    assert_eq!(field(&m.run("admin", cmd), "nModified"), &Value::Int32(1));
}

/// What the parser of an update statement rejects fails the command; what the statement checks
/// when it runs is its write error (measured in `refrouter` 9.0.4).
#[test]
fn update_statements_fail_where_refdb_fails_them() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(0..2));
    let set = || d(&[("$set", d(&[("y", 1.into())]))]);
    let good = d(&[("q", d(&[("_id", 0.into())])), ("u", set())]);
    let run = |m: &mut Mem, extra: (&str, Value)| {
        let stmt = d(&[("q", d(&[])), ("u", set()), extra]);
        m.run("db", doc(&[("update", s("c")), ("updates", Value::Array(vec![good.clone(), stmt]))]))
    };
    for (extra, code_of, msg) in [
        (("zz", 1.into()), 40415, "BSON field 'update.updates.zz' is an unknown field."),
        (
            ("multi", s("x")),
            14,
            "BSON field 'update.updates.multi' is the wrong type 'string', expected type 'bool'",
        ),
        (
            ("arrayFilters", 5.into()),
            14,
            "BSON field 'update.updates.arrayFilters' is the wrong type 'int', expected type 'array'",
        ),
    ] {
        let r = run(&mut m, extra);
        assert_eq!((code(&r), field(&r, "errmsg")), (code_of, &s(msg)));
    }
    assert_eq!(
        field(&m.run("db", doc(&[("count", s("c")), ("query", d(&[("y", 1.into())]))])), "n"),
        &Value::Int32(0)
    );
    for (extra, code_of, msg) in [
        (
            ("c", d(&[("a", 1.into())])),
            51198,
            "Constant values may only be specified for pipeline updates",
        ),
        (
            ("upsertSupplied", true.into()),
            9,
            "cannot specify 'upsertSupplied: true' for a non-upsert operation",
        ),
    ] {
        let r = run(&mut m, extra);
        assert_eq!(field(&r, "n"), &Value::Int32(1));
        assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(1));
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(code_of));
        assert_eq!(field(&r, "writeErrors.0.errmsg"), &s(msg));
    }
    for extra in [("hint", d(&[("_id", 1.into())])), ("sort", d(&[("_id", 1.into())]))] {
        assert!(run(&mut m, extra).get("writeErrors").is_none());
    }
    // Constants go with a pipeline.
    let pipeline = Value::Array(vec![d(&[("$set", d(&[("y", 3.into())]))])]);
    let stmt = d(&[("q", d(&[])), ("u", pipeline), ("c", d(&[("x", 1.into())]))]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", Value::Array(vec![stmt]))]));
    assert_ne!(
        r.get("writeErrors").map(|_| field(&r, "writeErrors.0.code")),
        Some(&Value::Int32(51198))
    );
    let replace = d(&[("q", d(&[])), ("u", d(&[("y", 2.into())])), ("multi", true.into())]);
    // Unordered, the statements after a failed one still run.
    let unordered = doc(&[
        ("update", s("c")),
        ("updates", Value::Array(vec![replace.clone(), good.clone()])),
        ("ordered", false.into()),
    ]);
    let r = m.run("db", unordered);
    assert_eq!(
        (field(&r, "n"), field(&r, "writeErrors.0.index")),
        (&Value::Int32(1), &Value::Int32(0))
    );
    let r = m.run("db", doc(&[("update", s("c")), ("updates", Value::Array(vec![good, replace]))]));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("multi update is not supported for replacement-style update")
    );
}

/// The conflicts of `findAndModify`'s options, with the messages of a `refrouter` 9.0.4.
#[test]
fn find_and_modify_refuses_remove_with_update_upsert_or_new() {
    let mut m = Mem::new();
    let base = || vec![("findAndModify", s("c")), ("query", d(&[])), ("remove", true.into())];
    for (extra, msg) in [
        (
            ("update", d(&[("$set", d(&[("a", 1.into())]))])),
            "Cannot specify both an 'update' and 'remove'=true",
        ),
        (("upsert", true.into()), "Cannot specify both 'upsert'=true and 'remove'=true "),
        (
            ("new", true.into()),
            "Cannot specify both 'new'=true and 'remove'=true; 'remove' always returns the deleted document",
        ),
    ] {
        let mut pairs = base();
        pairs.push(extra);
        let r = m.run("db", doc(&pairs));
        assert_eq!((code(&r), field(&r, "errmsg")), (9, &s(msg)));
    }
}

/// The validation options of `create` and `collMod` name their command in their messages, as a
/// `refrouter` 9.0.4 does.
#[test]
fn validation_options_name_their_command() {
    let mut m = Mem::new();
    m.run("db", doc(&[("create", s("v"))]));
    for (cmd, coll) in [("create", "w"), ("collMod", "v")] {
        for field in ["validationLevel", "validationAction"] {
            let r = m.run("db", doc(&[(cmd, s(coll)), (field, s("bogus"))]));
            let msg = format!(
                "Enumeration value 'bogus' for field '{cmd}.{field}' is not a valid value."
            );
            assert_eq!((code(&r), field_of(&r)), (2, msg));
        }
        let r = m.run("db", doc(&[(cmd, s(coll)), ("validationLevel", 5.into())]));
        let msg = format!(
            "BSON field '{cmd}.validationLevel' is the wrong type 'int', expected type 'string'"
        );
        assert_eq!((code(&r), field_of(&r)), (14, msg));
    }
}

fn field_of(r: &Document) -> String {
    match r.get("errmsg") {
        Some(Value::String(m)) => m.clone(),
        other => panic!("{other:?}"),
    }
}

/// The ObjectIds an insert gives (the ObjectId specification): the seconds, five random bytes of
/// the node, and a counter that grows from a random start, so that they sort in the order the
/// documents came.
#[test]
fn generated_ids_keep_the_order_of_the_inserts() {
    let mut m = Mem::new();
    let docs: Vec<Value> =
        (0..50).map(|i| Value::Document(doc(&[("i", Value::Int32(i))]))).collect();
    m.run("db", Document::new().with("insert", "c").with("documents", Value::Array(docs)));
    m.run(
        "db",
        Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(doc(&[("i", Value::Int32(50))]))]),
    );
    let r = m.run(
        "db",
        Document::new()
            .with("find", "c")
            .with("sort", doc(&[("_id", 1.into())]))
            .with("batchSize", 100),
    );
    let Value::Document(cursor) = field(&r, "cursor") else { panic!("{r:?}") };
    let Some(Value::Array(found)) = cursor.get("firstBatch") else { panic!("{r:?}") };
    let order: Vec<Value> = found
        .iter()
        .map(|d| match d {
            Value::Document(d) => d.get("i").cloned().unwrap_or(Value::Null),
            other => other.clone(),
        })
        .collect();
    let want: Vec<Value> = (0..51).map(Value::Int32).collect();
    assert_eq!(order, want);
    let ids: Vec<[u8; 12]> = found
        .iter()
        .filter_map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::ObjectId(id)) => Some(*id),
                _ => None,
            },
            _ => None,
        })
        .collect();
    // One node, one random part.
    assert!(ids.windows(2).all(|w| w[0][4..9] == w[1][4..9]));
}

/// A single `bulkWrite` update or replacement takes the first matching document in its `sort`
/// order, and `sort` with `multi` fails that operation alone (measured on a `refrouter` 9.0.4 on
/// 03-11-2026, and the drivers' `client-bulkWrite-*-sort` tests).
#[test]
fn bulk_write_updates_follow_their_sort() {
    let mut m = Mem::new();
    insert(&mut m, "c", ids(1..4));
    let ns_info = Value::Array(vec![d(&[("ns", s("db.c"))])]);
    let bulk = |op: Value| {
        doc(&[
            ("bulkWrite", 1.into()),
            ("ops", Value::Array(vec![op])),
            ("nsInfo", ns_info.clone()),
        ])
    };
    let gt1 = || d(&[("_id", d(&[("$gt", 1.into())]))]);
    let sort = || d(&[("_id", (-1).into())]);
    let r = m.run(
        "admin",
        bulk(d(&[
            ("update", 0.into()),
            ("filter", gt1()),
            ("updateMods", d(&[("$set", d(&[("x", 1.into())]))])),
            ("sort", sort()),
        ])),
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    let r = m.run(
        "admin",
        bulk(d(&[
            ("update", 0.into()),
            ("filter", gt1()),
            ("updateMods", d(&[("y", 2.into())])),
            ("sort", d(&[("_id", 1.into())])),
        ])),
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    let r = m.run("db", doc(&[("find", s("c")), ("sort", d(&[("_id", 1.into())]))]));
    assert_eq!(
        first_batch(&r),
        vec![
            d(&[("_id", 1.into()), ("a", 1.into())]),
            d(&[("_id", 2.into()), ("y", 2.into())]),
            d(&[("_id", 3.into()), ("a", 0.into()), ("x", 1.into())]),
        ]
    );
    let r = m.run(
        "admin",
        bulk(d(&[
            ("update", 0.into()),
            ("filter", gt1()),
            ("updateMods", d(&[("$set", d(&[("z", 1.into())]))])),
            ("multi", true.into()),
            ("sort", sort()),
        ])),
    );
    assert_eq!(field(&r, "nErrors"), &Value::Int32(1));
    assert_eq!(field(&r, "cursor.firstBatch.0.code"), &Value::Int32(9));
    assert_eq!(field(&r, "cursor.firstBatch.0.errmsg"), &s("Cannot specify sort with multi=true"));
}
