//! Edge cases of the CRUD and catalog commands of 006: malformed options, the limits of a
//! batch and of a cursor, collations, validators that warn, mutations that do not decode.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_hlc::Hlc;
use teseladb_range::host::Mutation;
use teseladb_wire::commands::mutation::{Bound, DocMutation, Image, MutationResult, Rules};
use teseladb_wire::error::CommandError;

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

fn many(n: i32, pad: usize) -> Value {
    Value::Array((0..n).map(|i| d(&[("_id", i.into()), ("p", s(&"x".repeat(pad)))])).collect())
}

fn with(base: &[(&str, Value)], extra: (&str, Value)) -> Document {
    let mut c = doc(base);
    c.push(extra.0, extra.1);
    c
}

#[test]
fn malformed_options_fail_with_refdb_codes() {
    let mut m = Mem::new();
    m.run("db", doc(&[("insert", s("c")), ("documents", many(3, 1))]));
    let find = [("find", s("c"))];
    for (opt, want) in [
        (("skip", s("x")), 14),
        (("singleBatch", s("x")), 14),
        (("filter", 1.into()), 14),
        (("readConcern", d(&[("level", s("bogus"))])), 2),
        (("readConcern", d(&[("level", 1.into())])), 14),
        (("readConcern", 1.into()), 14),
        (("lsid", 1.into()), 14),
        (("lsid", d(&[("id", 1.into())])), 14),
        (("lsid", d(&[])), 40414),
        (("collation", d(&[("locale", s("xx_bogus"))])), 2),
    ] {
        let r = m.run("db", with(&find, opt.clone()));
        assert_eq!(r.get("code"), Some(&Value::Int32(want)), "{opt:?}: {r:?}");
    }
    for level in ["local", "available", "majority", "linearizable", "snapshot"] {
        let r = m.run("db", with(&find, ("readConcern", d(&[("level", s(level))]))));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    }
    let r = m
        .run("db", with(&[("insert", s("c")), ("documents", many(1, 1))], ("txnNumber", 1.into())));
    assert_eq!(code(&r), 14);
    let ins = [("insert", s("c")), ("documents", Value::Array(vec![d(&[("_id", 50.into())])]))];
    for wc in [
        d(&[("w", s("majority"))]),
        d(&[("w", Value::Double(1.0)), ("j", false.into())]),
        d(&[("w", 0.into())]),
        d(&[]),
    ] {
        let r = m.run("db", with(&ins, ("writeConcern", wc)));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    }
    assert_eq!(code(&m.run("db", with(&ins, ("writeConcern", 1.into())))), 14);
    assert_eq!(code(&m.run("db", with(&ins, ("ordered", s("x"))))), 14);
    assert_eq!(code(&m.run("db", doc(&[("insert", s("c")), ("documents", 1.into())]))), 14);
}

#[test]
fn inserted_documents_and_ids_are_checked() {
    let mut m = Mem::new();
    let bad = Value::Array(vec![
        1.into(),
        d(&[("_id", Value::Regex { pattern: "a".into(), options: String::new() })]),
        d(&[("_id", Value::Undefined)]),
        d(&[("_id", d(&[("$x", 1.into())]))]),
        d(&[("x", 1.into()), ("_id", 7.into())]),
    ]);
    let r = m.run("db", doc(&[("insert", s("c")), ("documents", bad), ("ordered", false.into())]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(14));
    assert_eq!(field(&r, "writeErrors.1.errmsg"), &s("The '_id' value cannot be of type regex"));
    assert_eq!(field(&r, "writeErrors.3.code"), &Value::Int32(52));
    // `_id` goes first in the stored document.
    let r = m.run("db", doc(&[("find", s("c"))]));
    assert_eq!(field(&r, "cursor.firstBatch.0"), &d(&[("_id", 7.into()), ("x", 1.into())]));
    // An ordered insert stops at the first bad document.
    let r = m
        .run("db", doc(&[("insert", s("c")), ("documents", Value::Array(vec![1.into(), d(&[])]))]));
    assert_eq!(
        (field(&r, "n"), field(&r, "writeErrors.0.index")),
        (&Value::Int32(0), &Value::Int32(0))
    );
    // A document over 16 MiB.
    let big = Value::Array(vec![d(&[("_id", 1.into()), ("p", s(&"x".repeat(17 << 20)))])]);
    let r = m.run("db", doc(&[("insert", s("c")), ("documents", big)]));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(10334));
}

/// A stored document of exactly `size` bytes: `_id` and one string.
fn of_size(id: i32, size: usize) -> Value {
    d(&[("_id", id.into()), ("s", s(&"a".repeat(size - 22)))])
}

/// `levels` nested documents `{a: {a: … {x: 1}}}`, the outer one included.
fn nested(levels: usize) -> Value {
    (1..levels).fold(d(&[("x", 1.into())]), |inner, _| d(&[("a", inner)]))
}

#[test]
fn stored_limits_answer_as_a_refrouter() {
    // Measured against a `refrouter` 9.0.4 (003 T009): 16 MiB exactly and 180 levels are stored.
    let mut m = Mem::new();
    let insert = |docs: Vec<Value>| doc(&[("insert", s("c")), ("documents", Value::Array(docs))]);
    let r = m.run("db", insert(vec![of_size(1, 16 << 20)]));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = m.run("db", insert(vec![of_size(2, (16 << 20) + 1)]));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("object to insert too large. size in bytes: 16777217, max size: 16777216")
    );
    let r = m.run("db", insert(vec![d(&[("_id", 3.into()), ("a", nested(179))])]));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = m.run("db", insert(vec![d(&[("_id", 4.into()), ("a", nested(180))])]));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(15));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("cannot insert document :: caused by :: object exceeds 180 levels of nesting")
    );
    // An update whose result is too deep or too large, and an upsert.
    let update = |q: i32, u: Value, upsert: bool| {
        let stmt = d(&[("q", d(&[("_id", q.into())])), ("u", u), ("upsert", upsert.into())]);
        doc(&[("update", s("c")), ("updates", Value::Array(vec![stmt]))])
    };
    let r = m.run("db", update(3, d(&[("$set", d(&[("b", nested(180))]))]), false));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("Plan executor error during update :: caused by :: Document exceeds maximum nesting \
            depth of 180")
    );
    let r = m.run("db", update(3, d(&[("$set", d(&[("t", s(&"b".repeat(16 << 20)))]))]), false));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(10334));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("Plan executor error during update :: caused by :: Resulting document after update is \
            larger than 16777216")
    );
    let r = m.run("db", update(9, d(&[("a", nested(180))]), true));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(15));
    let r = m.run("db", update(9, of_size(9, (16 << 20) + 1), true));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("Plan executor error during update :: caused by :: Document to upsert is larger than \
            16777216")
    );
}

#[test]
fn big_batches_go_in_several_mutations() {
    let mut m = Mem::new();
    let r = m.run("db", doc(&[("insert", s("c")), ("documents", many(2500, 1))]));
    assert_eq!(field(&r, "n"), &Value::Int32(2500));
    assert!(m.writes >= 3);
    // Duplicates in the second chunk of an ordered insert stop it there.
    let mut docs: Vec<Value> = (3000..4200).map(|i| d(&[("_id", i.into())])).collect();
    docs[1100] = d(&[("_id", 1.into())]);
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            ("documents", Value::Array(docs)),
            ("stmtIds", Value::Array(vec![])),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1100));
    // Chunks also close by size.
    let r = m.run("db", doc(&[("insert", s("big")), ("documents", many(12, 2 << 20))]));
    assert_eq!(field(&r, "n"), &Value::Int32(12));
    // A multi update and a count over more keys than one mutation checks.
    let up = d(&[
        ("q", d(&[("p", s("x"))])),
        ("u", d(&[("$set", d(&[("k", 1.into())]))])),
        ("multi", true.into()),
    ]);
    let r = m.run(
        "db",
        doc(&[("update", s("c")), ("updates", Value::Array(vec![up])), ("stmtId", 4.into())]),
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(2500));
    let r = m.run("db", doc(&[("count", s("c")), ("query", d(&[("k", 1.into())]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(2500));
    // A find walks every chunk; big documents end a batch at 16 MiB.
    let r = m.run("db", doc(&[("find", s("c")), ("batchSize", 3000.into()), ("skip", 10.into())]));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!() };
    assert_eq!(b.len(), 3000);
    let r = m.run("db", doc(&[("find", s("big"))]));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!() };
    assert!(b.len() < 12);
    let Value::Int64(id) = field(&r, "cursor.id").clone() else { panic!() };
    let r = m.run("db", doc(&[("getMore", Value::Int64(id)), ("collection", s("big"))]));
    assert!(matches!(field(&r, "cursor.nextBatch"), Value::Array(x) if !x.is_empty()));
}

#[test]
fn collations_rules_and_warnings_reach_the_leader() {
    let mut m = Mem::new();
    let col = d(&[("locale", s("en")), ("strength", 1.into())]);
    let r = m.run("db", doc(&[("create", s("c")), ("collation", col.clone())]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    assert_eq!(
        code(&m.run("db", doc(&[("create", s("x")), ("collation", d(&[("locale", s("zz"))]))]))),
        2
    );
    m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            (
                "documents",
                Value::Array(vec![
                    d(&[("_id", 1.into()), ("n", s("Ana"))]),
                    d(&[("_id", 2.into()), ("n", s("bob"))]),
                ]),
            ),
        ]),
    );
    // The collection's collation applies.
    let r = m.run("db", doc(&[("count", s("c")), ("query", d(&[("n", s("ANA"))]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    let r = m.run(
        "db",
        doc(&[
            ("find", s("c")),
            ("sort", d(&[("n", (-1).into())])),
            ("collation", d(&[("locale", s("simple"))])),
        ]),
    );
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(2));
    let r = m.run("db", doc(&[("distinct", s("c")), ("key", s("n")), ("collation", col)]));
    assert!(matches!(field(&r, "values"), Value::Array(v) if v.len() == 2));
    let up = d(&[("q", d(&[("n", s("BOB"))])), ("u", d(&[("$set", d(&[("k", 1.into())]))]))]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", Value::Array(vec![up]))]));
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    // A validator that warns lets the write through.
    let v = d(&[("a", d(&[("$exists", true.into())]))]);
    m.run("db", doc(&[("create", s("w")), ("validator", v), ("validationAction", s("warn"))]));
    let r = m.run(
        "db",
        doc(&[("insert", s("w")), ("documents", Value::Array(vec![d(&[("_id", 1.into())])]))]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    // A `moderate` validator skips documents that were already invalid.
    m.run(
        "db",
        doc(&[
            ("collMod", s("w")),
            ("validationLevel", s("moderate")),
            ("validationAction", s("error")),
        ]),
    );
    let up = d(&[("q", d(&[("_id", 1.into())])), ("u", d(&[("$set", d(&[("z", 1.into())]))]))]);
    let r = m.run("db", doc(&[("update", s("w")), ("updates", Value::Array(vec![up]))]));
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    assert_eq!(code(&m.run("db", doc(&[("create", s("y")), ("validationAction", 1.into())]))), 14);
    assert_eq!(code(&m.run("db", doc(&[("create", s("y")), ("validationLevel", 1.into())]))), 14);
    assert_eq!(code(&m.run("db", doc(&[("create", s("y")), ("validationAction", s("x"))]))), 2);
    assert_eq!(
        code(&m.run("db", doc(&[("create", s("y")), ("validator", d(&[("$text", d(&[]))]))]))),
        2
    );
    assert_eq!(code(&m.run("db", doc(&[("create", s("system.y"))]))), 73);
}

#[test]
fn find_and_modify_and_bulk_write_cover_their_options() {
    let mut m = Mem::new();
    m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            (
                "documents",
                Value::Array(vec![
                    d(&[("_id", 1.into()), ("a", Value::Array(vec![1.into(), 5.into()]))]),
                    d(&[("_id", 2.into())]),
                ]),
            ),
        ]),
    );
    let r = m.run(
        "db",
        doc(&[
            ("findandmodify", s("c")),
            ("query", d(&[("a", d(&[("$exists", true.into())]))])),
            ("sort", d(&[("_id", 1.into())])),
            ("update", d(&[("$set", d(&[("a.$[e]", 0.into())]))])),
            ("arrayFilters", Value::Array(vec![d(&[("e", d(&[("$gt", 2.into())]))])])),
            ("new", true.into()),
            ("bypassDocumentValidation", true.into()),
        ]),
    );
    assert_eq!(field(&r, "value.a"), &Value::Array(vec![1.into(), 0.into()]));
    assert_eq!(
        code(&m.run(
            "db",
            doc(&[("findAndModify", s("c")), ("update", d(&[])), ("sort", d(&[("a", 3.into())]))])
        )),
        15975
    );
    // An update that fails on the leader keeps RefDb's code.
    let r = m.run(
        "db",
        doc(&[
            ("findAndModify", s("c")),
            ("query", d(&[("_id", 2.into())])),
            ("update", d(&[("$set", d(&[("_id", 3.into())]))])),
        ]),
    );
    assert_eq!(code(&r), 66);
    // An upsert whose `_id` collides with a document that does not match.
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            (
                "updates",
                Value::Array(vec![d(&[
                    ("q", d(&[("_id", 2.into()), ("z", 1.into())])),
                    ("u", d(&[("$set", d(&[("y", 1.into())]))])),
                    ("upsert", true.into()),
                ])]),
            ),
        ]),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000));
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            ("updates", Value::Array(vec![1.into(), d(&[("u", d(&[]))])])),
            ("ordered", false.into()),
        ]),
    );
    // A statement the parser rejects fails the whole command, even unordered (measured).
    assert_eq!(code(&r), 14);
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'update.updates.0' is the wrong type 'int', expected type 'object'")
    );
    let r = m.run(
        "db",
        doc(&[
            ("delete", s("c")),
            ("deletes", Value::Array(vec![1.into(), d(&[("q", d(&[]))])])),
            ("ordered", false.into()),
        ]),
    );
    assert_eq!(code(&r), 14);
    let ns = Value::Array(vec![d(&[("ns", s("db.c"))])]);
    let errors_only = doc(&[
        ("bulkWrite", 1.into()),
        (
            "ops",
            Value::Array(vec![d(&[
                ("update", 0.into()),
                ("filter", d(&[("_id", 1.into())])),
                ("updateMods", d(&[("$set", d(&[("q", 1.into())]))])),
            ])]),
        ),
        ("nsInfo", ns.clone()),
        ("errorsOnly", true.into()),
    ]);
    let r = m.run("admin", errors_only);
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    let ordered = doc(&[
        ("bulkWrite", 1.into()),
        ("ops", Value::Array(vec![1.into(), d(&[])])),
        ("nsInfo", ns),
    ]);
    assert_eq!(code(&m.run("admin", ordered)), 14);
    for info in [
        Value::Array(vec![1.into()]),
        Value::Array(vec![d(&[])]),
        Value::Array(vec![d(&[("ns", s("nodot"))])]),
    ] {
        let r = m.run(
            "admin",
            doc(&[("bulkWrite", 1.into()), ("ops", Value::Array(vec![])), ("nsInfo", info)]),
        );
        // The fields first, then the empty batch before the namespace (measured on 14-11-2026).
        assert!(matches!(r.get("code"), Some(Value::Int32(14 | 40414 | 16))), "{r:?}");
    }
}

#[test]
fn catalog_commands_check_their_fields() {
    let mut m = Mem::new();
    m.run("db", doc(&[("create", s("c"))]));
    assert_eq!(
        code(&m.run("admin", doc(&[("renameCollection", 1.into()), ("to", s("db.d"))]))),
        14
    );
    assert_eq!(code(&m.run("admin", doc(&[("renameCollection", s("db.c"))]))), 40414);
    // Across databases: the target is `db2.d`.
    assert_eq!(
        field(&m.run("admin", doc(&[("renameCollection", s("db.c")), ("to", s("db2.d"))])), "ok"),
        &Value::Double(1.0)
    );
    let r = m.run(
        "db2",
        doc(&[("listCollections", 1.into()), ("cursor", d(&[("batchSize", 0.into())]))]),
    );
    let Value::Int64(id) = field(&r, "cursor.id").clone() else { panic!() };
    assert_ne!(id, 0);
    let r = m.run(
        "db2",
        doc(&[("getMore", Value::Int64(id)), ("collection", s("$cmd.listCollections"))]),
    );
    assert!(matches!(field(&r, "cursor.nextBatch"), Value::Array(x) if x.len() == 1));
    let r =
        m.run("admin", doc(&[("listDatabases", 1.into()), ("filter", d(&[("name", s("none"))]))]));
    assert_eq!(field(&r, "databases"), &Value::Array(vec![]));
    assert_eq!(code(&m.run("a.b", doc(&[("dropDatabase", 1.into())]))), 73);
    assert_eq!(
        code(&m.run(
            "db2",
            doc(&[("listCollections", 1.into()), ("filter", d(&[("$bad", 1.into())]))])
        )),
        2
    );
}

#[test]
fn mutations_that_do_not_decode_are_internal_errors() {
    assert!(Bound::decode(&[1, 2, 3]).is_err());
    let empty = Document::new().to_bytes().unwrap();
    assert!(Bound::decode(&empty).is_err());
    let keys_not_binary = doc(&[
        ("keys", Value::Array(vec![1.into()])),
        ("m", Value::Binary { subtype: 0, bytes: empty.clone() }),
    ]);
    assert!(Bound::decode(&keys_not_binary.to_bytes().unwrap()).is_err());
    assert!(DocMutation::decode(&[0]).is_err());
    assert!(DocMutation::decode(&empty).is_err());
    let docs_not_docs = doc(&[("k", 0.into()), ("docs", Value::Array(vec![1.into()]))]);
    assert!(DocMutation::decode(&docs_not_docs.to_bytes().unwrap()).is_err());
    let no_update = doc(&[("k", 1.into()), ("filter", d(&[]))]);
    assert!(DocMutation::decode(&no_update.to_bytes().unwrap()).is_err());
    assert!(MutationResult::decode(&[0]).is_err());
    let bad_errors = doc(&[("errors", Value::Array(vec![1.into()]))]);
    assert!(MutationResult::decode(&bad_errors.to_bytes().unwrap()).is_err());
    let short_error = doc(&[("errors", Value::Array(vec![d(&[("index", Value::Int64(0))])]))]);
    assert!(MutationResult::decode(&short_error.to_bytes().unwrap()).is_err());
    // A result with a code this crate does not know, and warnings that are not documents.
    let odd = doc(&[
        ("n", Value::Int64(1)),
        (
            "errors",
            Value::Array(vec![d(&[
                ("index", Value::Int64(0)),
                ("code", 4242.into()),
                ("errmsg", s("x")),
            ])]),
        ),
        ("warnings", Value::Array(vec![1.into()])),
    ]);
    let r = MutationResult::decode(&odd.to_bytes().unwrap()).unwrap();
    assert_eq!((r.n, r.errors[0].error.code.code, r.warnings.len()), (1, 1, 0));
    // Bad rules fail the statement inside the result.
    let rules = Rules {
        validator: Some((Document::new(), "nope".into(), "error".into())),
        ..Rules::default()
    };
    let b = Bound {
        mutation: DocMutation::Insert { docs: vec![Document::new()], ordered: true, rules },
        keys: vec![vec![1]],
    };
    let out = b.eval(&[None], Hlc { physical: 1, logical: 0 }).unwrap();
    assert_eq!(MutationResult::decode(&out.result).unwrap().errors.len(), 1);
    let rules = Rules { collation: Some(doc(&[("locale", s("zz"))])), ..Rules::default() };
    let b = Bound {
        mutation: DocMutation::Delete {
            filter: Document::new(),
            multi: true,
            image: Image::Before,
            rules,
        },
        keys: vec![vec![1]],
    };
    let out = b.eval(&[None], Hlc { physical: 1, logical: 0 }).unwrap();
    assert_eq!(MutationResult::decode(&out.result).unwrap().errors.len(), 1);
    let e: CommandError = teseladb_bson::BsonError::TooDeep { offset: 0, path: "a".into() }.into();
    assert_eq!(e.code.code, 15);
    assert_eq!(
        e.errmsg,
        "BSONObj exceeds maximum nested object depth in element with field name 'a'"
    );
}

#[test]
fn required_fields_are_reported() {
    let mut m = Mem::new();
    m.run("db", doc(&[("insert", s("c")), ("documents", many(2, 1))]));
    for (cmd, want) in [
        (doc(&[("update", s("c"))]), 40414),
        (doc(&[("delete", s("c"))]), 40414),
        (doc(&[("bulkWrite", 1.into()), ("nsInfo", Value::Array(vec![]))]), 40414),
        (doc(&[("create", s("c")), ("$foo", 1.into())]), 0),
    ] {
        let r = m.run(if want == 0 { "db" } else { "admin" }, cmd.clone());
        if want != 0 {
            assert_eq!(r.get("code"), Some(&Value::Int32(want)), "{cmd:?}");
        }
    }
    let r = m.run(
        "db",
        doc(&[("update", s("c")), ("updates", Value::Array(vec![d(&[("u", d(&[]))])]))]),
    );
    assert_eq!(code(&r), 40414);
    let r = m.run(
        "db",
        doc(&[("update", s("c")), ("updates", Value::Array(vec![d(&[("q", d(&[]))])]))]),
    );
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'update.updates.u' is missing but a required field")
    );
    let r = m.run(
        "db",
        doc(&[("delete", s("c")), ("deletes", Value::Array(vec![d(&[("limit", 1.into())])]))]),
    );
    assert_eq!(code(&r), 40414);
    let ns = Value::Array(vec![d(&[("ns", s("db.c"))])]);
    let ops = Value::Array(vec![
        d(&[]),
        d(&[("insert", 0.into())]),
        d(&[("update", 0.into())]),
        d(&[("delete", 0.into())]),
        d(&[
            ("update", 0.into()),
            ("filter", d(&[("_id", 99.into())])),
            ("updateMods", d(&[("$set", d(&[("a", 1.into())]))])),
        ]),
    ]);
    // Each operation the parser refuses fails the command on its own.
    let Value::Array(all) = ops else { unreachable!() };
    for (op, code_of) in all[..4].iter().zip([40415, 40414, 40414, 40414]) {
        let one = Value::Array(vec![op.clone()]);
        let cmd = doc(&[("bulkWrite", 1.into()), ("ops", one), ("nsInfo", ns.clone())]);
        assert_eq!(code(&m.run("admin", cmd)), code_of, "{op:?}");
    }
    let rest = Value::Array(all[4..].to_vec());
    let r = m.run(
        "admin",
        doc(&[("bulkWrite", 1.into()), ("ops", rest), ("nsInfo", ns), ("ordered", false.into())]),
    );
    assert_eq!(field(&r, "nErrors"), &Value::Int32(0));
    assert_eq!(field(&r, "nMatched"), &Value::Int32(0));
    // Equalities on `_id` find the document by its key; `$eq` too.
    let r = m.run(
        "db",
        doc(&[
            ("delete", s("c")),
            (
                "deletes",
                Value::Array(vec![d(&[
                    ("q", d(&[("_id", d(&[("$eq", 0.into())]))])),
                    ("limit", 1.into()),
                ])]),
            ),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    let r = m.run(
        "db",
        doc(&[
            ("delete", s("c")),
            (
                "deletes",
                Value::Array(vec![d(&[
                    ("q", d(&[("_id", d(&[("$in", Value::Array(vec![1.into()]))]))])),
                    ("limit", 1.into()),
                ])]),
            ),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    for q in [
        d(&[("_id", Value::Regex { pattern: "x".into(), options: String::new() })]),
        d(&[("_id", Value::Array(vec![]))]),
    ] {
        let r = m.run(
            "db",
            doc(&[
                ("delete", s("c")),
                ("deletes", Value::Array(vec![d(&[("q", q), ("limit", 1.into())])])),
            ]),
        );
        assert_eq!(field(&r, "n"), &Value::Int32(0));
    }
    // A find with an empty sort and projection, a sort with a limit, a projection on a sort.
    m.run(
        "db",
        doc(&[
            ("insert", s("s")),
            (
                "documents",
                Value::Array(vec![
                    d(&[("_id", 1.into()), ("a", Value::Array(vec![]))]),
                    d(&[
                        ("_id", 2.into()),
                        ("a", Value::Array(vec![d(&[("b", 3.into())]), d(&[("b", 1.into())])])),
                    ]),
                    d(&[("_id", 3.into())]),
                    d(&[("_id", 4.into()), ("a", Value::Array(vec![5.into(), 2.into()]))]),
                ]),
            ),
        ]),
    );
    let r = m.run("db", doc(&[("find", s("s")), ("sort", d(&[])), ("projection", d(&[]))]));
    assert!(matches!(field(&r, "cursor.firstBatch"), Value::Array(x) if x.len() == 4));
    let r = m.run(
        "db",
        doc(&[
            ("find", s("s")),
            ("sort", d(&[("a.b", 1.into()), ("a.0", (-1).into())])),
            ("limit", 2.into()),
            ("projection", d(&[("_id", 1.into())])),
            ("allowDiskUse", true.into()),
        ]),
    );
    // As a `refrouter` 7.0 sorts them (measured).
    assert_eq!(field(&r, "cursor.firstBatch.0"), &d(&[("_id", 4.into())]));
    for (dir, want) in [(1, [1, 3, 4, 2]), (-1, [2, 4, 3, 1])] {
        let r = m.run(
            "db",
            doc(&[
                ("find", s("s")),
                ("sort", d(&[("a", dir.into())])),
                ("projection", d(&[("_id", 1.into())])),
            ]),
        );
        let got: Vec<Value> = want.iter().map(|&i| d(&[("_id", i.into())])).collect();
        assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(got));
    }
    let r = m.run(
        "db",
        doc(&[
            ("find", s("s")),
            ("sort", d(&[("a", 1.into())])),
            ("batchSize", 1.into()),
            ("limit", 3.into()),
        ]),
    );
    let Value::Int64(id) = field(&r, "cursor.id").clone() else { panic!() };
    let r = m.run(
        "db",
        doc(&[("getMore", Value::Int64(id)), ("collection", s("s")), ("batchSize", 5.into())]),
    );
    assert_eq!(field(&r, "cursor.id"), &Value::Int64(0));
    let r = m.run(
        "db",
        doc(&[
            ("findAndModify", s("s")),
            ("query", d(&[])),
            ("sort", d(&[])),
            ("remove", true.into()),
            ("fields", d(&[])),
        ]),
    );
    assert!(matches!(field(&r, "value"), Value::Document(_)));
    let r = m.run("db", doc(&[("startSession", 1.into())]));
    let lsid = field(&r, "id").clone();
    assert_eq!(
        field(&m.run("db", doc(&[("killSessions", Value::Array(vec![lsid]))])), "ok"),
        &Value::Double(1.0)
    );
    assert_eq!(
        field(
            &m.run(
                "db",
                doc(&[(
                    "endSessions",
                    Value::Array(vec![d(&[(
                        "id",
                        Value::Binary { subtype: 4, bytes: vec![1; 3] }
                    )])])
                )])
            ),
            "code"
        ),
        &Value::Int32(14)
    );
    assert_eq!(
        field(&m.run("admin", doc(&[("renameCollection", s("db.s")), ("to", s("nodot"))])), "code"),
        &Value::Int32(73)
    );
    // A null character fails the whole command (measured on a `refrouter` 9.0.4).
    assert_eq!(
        field(&m.run("db", doc(&[("insert", s("a\0b")), ("documents", many(1, 1))])), "code"),
        &Value::Int32(73)
    );
}

#[test]
fn limits_of_batches_sorts_and_options() {
    let mut m = Mem::new();
    let docs: Vec<Value> = (0..100_001).map(|_| d(&[])).collect();
    let r = m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(docs))]));
    assert_eq!(field(&r, "code"), &Value::Int32(16));
    // Sorting more than 100 MiB with `allowDiskUse: false` fails; without it, it is allowed
    // (measured on a `refrouter` 9.0.4).
    m.run("db", doc(&[("insert", s("big")), ("documents", many(8, 14 << 20))]));
    let r = m.run(
        "db",
        doc(&[
            ("find", s("big")),
            ("sort", d(&[("_id", 1.into())])),
            ("allowDiskUse", false.into()),
        ]),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(292));
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "Executor error during find command: db.big :: caused by :: Sort exceeded memory limit of 104857600 bytes, but did not opt in to external sorting."
        )
    );
    let r = m.run(
        "db",
        doc(&[("find", s("big")), ("sort", d(&[("_id", 1.into())])), ("batchSize", 1.into())]),
    );
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(0));
    let r = m.run(
        "db",
        doc(&[
            ("find", s("big")),
            ("sort", d(&[("_id", 1.into())])),
            ("allowDiskUse", true.into()),
            ("batchSize", 1.into()),
        ]),
    );
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(0));
    // An infinite skip saturates, as in RefDb: nothing is left.
    let r = m.run("db", with(&[("find", s("c"))], ("skip", Value::Double(f64::INFINITY))));
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(Vec::new()));
    let ins = [("insert", s("c")), ("documents", many(1, 1))];
    for w in [Value::from(-1), Value::from(51)] {
        let r = m.run("db", with(&ins, ("writeConcern", d(&[("w", w)]))));
        assert_eq!(field(&r, "code"), &Value::Int32(9));
    }
    let r = m.run(
        "db",
        with(&ins, ("lsid", d(&[("id", Value::Binary { subtype: 4, bytes: vec![1; 3] })]))),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(14));
    // `_id` filters that are not plain equalities scan.
    m.run(
        "db",
        doc(&[
            ("insert", s("i")),
            ("documents", Value::Array(vec![d(&[("_id", d(&[("a", 1.into())]))])])),
        ]),
    );
    for q in [
        d(&[("_id", d(&[("a", 1.into())]))]),
        d(&[("_id", d(&[("$eq", d(&[("a", 1.into())])), ("$ne", 2.into())]))]),
    ] {
        let r = m.run(
            "db",
            doc(&[
                ("delete", s("i")),
                ("deletes", Value::Array(vec![d(&[("q", q), ("limit", 0.into())])])),
            ]),
        );
        assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    }
}

#[test]
fn dispatch_runs_the_data_commands_when_a_node_is_behind() {
    use teseladb_wire::commands::{Ctx, run};
    use teseladb_wire::connection::Connection;
    use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
    use teseladb_wire::msg::OpMsg;
    let mut m = Mem::new();
    let shared = std::mem::take(&mut m.shared);
    let node = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 1,
        keys: None,
        replica_set: None,
    };
    let mut conn = Connection::new(1);
    let mut ask = |m: &mut Mem, cmd: Document| {
        conn.receive(&OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap()));
        let req = conn.next_request().unwrap().unwrap();
        match dispatch_with(&mut conn, &req, &node, Some((m, &shared))) {
            Action::Reply(d) => d,
            other => panic!("{other:?}"),
        }
    };
    let r = ask(&mut m, doc(&[("insert", s("c")), ("documents", many(1, 1)), ("$db", s("db"))]));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    assert!(r.get("$clusterTime").is_some());
    let r = ask(&mut m, doc(&[("ping", 1.into()), ("$db", s("db"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let r = ask(&mut m, doc(&[("nosuch", 1.into()), ("$db", s("db"))]));
    assert_eq!(field(&r, "code"), &Value::Int32(59));
    let cmd = doc(&[("ping", 1.into())]);
    assert!(run(&mut Ctx { backend: &mut m, shared: &shared, db: "db", cmd: &cmd }).is_none());
}

/// Each boolean option takes the types its IDL type takes in `refrouter` 9.0.4: `find`'s only a
/// boolean, `bool` fields a boolean or `null`, `safeBool` fields also a number.
#[test]
fn boolean_options_take_the_types_refdb_takes() {
    let mut m = Mem::new();
    m.run("db", doc(&[("insert", s("c")), ("documents", many(1, 1))]));
    let find_msg =
        |f: &str, t: &str| format!("Field '{f}' should be a boolean value, but found: {t}");
    for v in [Value::Int32(1), Value::Null] {
        let r = m.run("db", with(&[("find", s("c"))], ("singleBatch", v.clone())));
        let t = if v == Value::Null { "null" } else { "int" };
        assert_eq!(field(&r, "errmsg"), &s(&find_msg("singleBatch", t)));
    }
    let ins = [("insert", s("c")), ("documents", many(1, 1))];
    let safe = "'[long, decimal, bool, int, double]'";
    for (cmd, f, v, msg) in [
        (&ins[..], "ordered", Value::Int32(1), "BSON field 'insert.ordered' is the wrong type 'int', expected type 'bool'".to_owned()),
        (&ins[..], "bypassDocumentValidation", s("x"), format!("BSON field 'insert.bypassDocumentValidation' is the wrong type 'string', expected types {safe}")),
        (&[("listCollections", 1.into())][..], "nameOnly", Value::Int32(1), "BSON field 'listCollections.nameOnly' is the wrong type 'int', expected type 'bool'".to_owned()),
        // RefDb r9.0.4 list_collections.idl names the command, `listCollections` (09-11-2026).
        (&[("listCollections", 1.into())][..], "filter", Value::Int32(1), "BSON field 'listCollections.filter' is the wrong type 'int', expected type 'object'".to_owned()),
        (&[("listDatabases", 1.into())][..], "nameOnly", s("x"), "BSON field 'listDatabases.nameOnly' is the wrong type 'string', expected types '[decimal, long, int, bool, double]'".to_owned()),
        (&[("findAndModify", s("c")), ("remove", true.into())][..], "new", s("x"), format!("BSON field 'findAndModify.new' is the wrong type 'string', expected types {safe}")),
    ] {
        let db = if cmd[0].0 == "listDatabases" { "admin" } else { "db" };
        let r = m.run(db, with(cmd, (f, v)));
        assert_eq!((code(&r), field(&r, "errmsg")), (14, &s(&msg)), "{f}");
    }
    // `null` is an absent `bool` or `safeBool`; a number is a `safeBool`.
    for (f, v) in [("ordered", Value::Null), ("bypassDocumentValidation", Value::Int32(1))] {
        assert_eq!(field(&m.run("db", with(&ins, (f, v))), "ok"), &Value::Double(1.0), "{f}");
    }
    let r = m.run("admin", with(&[("listDatabases", 1.into())], ("nameOnly", Value::Int32(1))));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let op = d(&[
        ("update", 0.into()),
        ("filter", d(&[])),
        ("updateMods", d(&[("$set", d(&[]))])),
        ("multi", 1.into()),
    ]);
    let bulk = doc(&[
        ("bulkWrite", 1.into()),
        ("ops", Value::Array(vec![op])),
        ("nsInfo", Value::Array(vec![d(&[("ns", s("db.c"))])])),
    ]);
    let r = m.run("admin", bulk);
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'bulkWrite.ops.multi' is the wrong type 'int', expected type 'bool'")
    );
}

/// `renameCollection`'s `dropTarget` is a boolean or the UUID the target must have.
#[test]
fn drop_target_is_a_boolean_or_the_target_uuid() {
    let mut m = Mem::new();
    for c in ["a", "b"] {
        m.run("db", doc(&[("insert", s(c)), ("documents", many(1, 1))]));
    }
    let rename =
        |dt: Value| doc(&[("renameCollection", s("db.a")), ("to", s("db.b")), ("dropTarget", dt)]);
    let r = m.run("admin", rename(Value::Null));
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "BSON field 'renameCollection.dropTarget' is the wrong type 'null', expected types '[bool, binData]'"
        )
    );
    let r = m.run("admin", rename(Value::Binary { subtype: 0, bytes: vec![0; 3] }));
    assert_eq!(code(&r), 207);
    let r = m.run("admin", rename(Value::Binary { subtype: 4, bytes: vec![7; 16] }));
    assert_eq!(
        (code(&r), field(&r, "errmsg")),
        (361, &s("Collection UUID does not match that specified"))
    );
    let lc = m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("b"))]))]));
    let uuid = field(&lc, "cursor.firstBatch.0.info.uuid").clone();
    assert_eq!(field(&m.run("admin", rename(uuid)), "ok"), &Value::Double(1.0));
    let r = m.run(
        "admin",
        doc(&[
            ("renameCollection", s("db.b")),
            ("to", s("db.z")),
            ("dropTarget", Value::Binary { subtype: 4, bytes: vec![7; 16] }),
        ]),
    );
    assert_eq!(code(&r), 361, "a target that does not exist has no UUID to match");
}

#[test]
fn a_sort_without_index_orders_as_a_refrouter() {
    // Measured on a `refrouter` 9.0.4 (003 T012): an empty array sorts as `undefined` (after MinKey,
    // before null, ties by `_id`), and DBPointers of one size go by ObjectId before namespace.
    let mut m = Mem::new();
    let p = |ns: &str, id: u8| Value::DbPointer { namespace: ns.into(), id: [id; 12] };
    let rows = [
        (2, Value::Array(vec![])),
        (1, Value::Undefined),
        (0, Value::MinKey),
        (3, Value::Null),
        (4, Value::Array(vec![d(&[])])),
        (5, Value::Array(vec![Value::Undefined])),
        (6, p("db.a", 0xFF)),
        (7, p("db.b", 0)),
        (8, p("db.aa", 0)),
    ];
    let docs = rows.iter().map(|(i, v)| d(&[("_id", (*i).into()), ("v", v.clone())])).collect();
    m.run("db", doc(&[("insert", s("e")), ("documents", Value::Array(docs))]));
    let order = |m: &mut Mem, dir: i32| {
        let sort = d(&[("v", dir.into()), ("_id", 1.into())]);
        let r = m.run("db", doc(&[("find", s("e")), ("sort", sort)]));
        let Value::Array(batch) = field(&r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
        batch
            .iter()
            .map(|x| match x {
                Value::Document(x) => x.get("_id").cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            })
            .collect::<Vec<_>>()
    };
    let ids = |v: &[i32]| v.iter().map(|&i| Value::Int32(i)).collect::<Vec<_>>();
    assert_eq!(order(&mut m, 1), ids(&[0, 1, 2, 5, 3, 4, 7, 6, 8]));
    assert_eq!(order(&mut m, -1), ids(&[8, 6, 7, 4, 3, 1, 2, 5, 0]));
    // An array's smallest or largest element is chosen in the order of queries (namespace
    // first), and then sorts as a DBPointer sorts (ObjectId first).
    let pair = Value::Array(vec![p("db.b", 0), p("db.a", 0xFF)]);
    let docs =
        vec![d(&[("_id", 1.into()), ("v", pair)]), d(&[("_id", 2.into()), ("v", p("db.c", 0x80))])];
    m.run("db", doc(&[("insert", s("f")), ("documents", Value::Array(docs))]));
    for dir in [1, -1] {
        let r = m.run("db", doc(&[("find", s("f")), ("sort", d(&[("v", dir.into())]))]));
        assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(2), "{dir}");
    }
}

#[test]
fn a_sort_on_a_path_through_arrays_orders_as_a_refrouter() {
    // Measured on a `refrouter` 9.0.4 (002 FR-031): an empty array reached through an array sorts
    // as `undefined`, an element where the path ends (a document without the field, a scalar, a
    // nested array) gives `null`, in `find` and in `$sort` alike.
    let mut m = Mem::new();
    let a = |v: Value| d(&[("a", v)]);
    let rows = [
        (1, Value::Array(vec![a(Value::Array(vec![])), a(1.into())])),
        (2, Value::Array(vec![d(&[]), a(1.into())])),
        (3, a(Value::Null)),
        (4, Value::Array(vec![5.into(), a(2.into())])),
        (5, Value::Array(vec![Value::Array(vec![a(0.into())]), a(3.into())])),
        (6, Value::Array(vec![a(Value::Array(vec![Value::Array(vec![]), 0.into()]))])),
        (8, a(Value::Array(vec![]))),
        (9, Value::Array(vec![a((-5).into())])),
    ];
    let mut docs: Vec<Value> =
        rows.iter().map(|(i, v)| d(&[("_id", (*i).into()), ("v", v.clone())])).collect();
    docs.push(d(&[("_id", 7.into())]));
    m.run("db", doc(&[("insert", s("p")), ("documents", Value::Array(docs))]));
    let ids_of = |r: &Document, at: &str| {
        let Value::Array(batch) = field(r, at).clone() else { panic!("{r:?}") };
        batch
            .iter()
            .map(|x| match x {
                Value::Document(x) => x.get("_id").cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            })
            .collect::<Vec<_>>()
    };
    let ids = |v: &[i32]| v.iter().map(|&i| Value::Int32(i)).collect::<Vec<_>>();
    for (dir, want) in [(1, [1, 8, 2, 3, 4, 5, 7, 9, 6]), (-1, [6, 5, 4, 1, 2, 9, 3, 7, 8])] {
        let sort = d(&[("v.a", dir.into()), ("_id", 1.into())]);
        let r = m.run("db", doc(&[("find", s("p")), ("sort", sort.clone())]));
        assert_eq!(ids_of(&r, "cursor.firstBatch"), ids(&want), "find {dir}");
        let pipeline = Value::Array(vec![d(&[("$sort", sort)])]);
        let r =
            m.run("db", doc(&[("aggregate", s("p")), ("pipeline", pipeline), ("cursor", d(&[]))]));
        assert_eq!(ids_of(&r, "cursor.firstBatch"), ids(&want), "$sort {dir}");
    }
}

/// A mutation or result past the limits of a range entry is an error with its code, never an
/// empty encoding; a result that cannot be kept writes nothing and answers with that error.
#[test]
fn encodings_past_the_entry_limits_are_errors() {
    let big = |mib: usize| Document::new().with("_id", 1).with("a", "x".repeat(mib << 20));
    let too_large = 10334;
    let insert =
        DocMutation::Insert { docs: vec![big(65)], ordered: true, rules: Rules::default() };
    assert_eq!(insert.encode().map(|_| ()).unwrap_err().code.code, too_large);
    let b = Bound { mutation: insert, keys: vec![vec![1]] };
    assert_eq!(b.encode().map(|_| ()).unwrap_err().code.code, too_large);
    let r = MutationResult { warnings: vec![big(65)], ..MutationResult::default() };
    assert_eq!(r.encode().map(|_| ()).unwrap_err().code.code, too_large);
    // A validator that warns on two documents of 33 MiB: its warnings do not fit in an entry.
    let rules = Rules {
        validator: Some((doc(&[("a", d(&[("$type", s("int"))]))]), "strict".into(), "warn".into())),
        ..Rules::default()
    };
    let mut second = big(33);
    second.insert("_id", 2);
    let docs = vec![big(33), second];
    let mutation = DocMutation::Insert { docs, ordered: true, rules };
    let b = Bound { mutation, keys: vec![vec![1], vec![2]] };
    let out = b.eval(&[None, None], Hlc { physical: 1, logical: 0 }).unwrap();
    let r = MutationResult::decode(&out.result).unwrap();
    assert!(out.puts.is_empty() && out.deletes.is_empty());
    assert_eq!((r.n, r.errors[0].error.code.code), (0, too_large));
}

/// Each top-level `Timestamp(0, 0)` but `_id` takes the write's time, one increment apart, on an
/// insert and a replacement; nested ones, `$set` and an upsert by operators keep it (measured).
#[test]
fn empty_timestamps_take_the_time_of_the_write() {
    let z = Value::Timestamp { time: 0, increment: 0 };
    let ts = Hlc { physical: 5_000, logical: 2 };
    let (time, inc) = teseladb_hlc::to_timestamp(ts);
    let stored = |out: &teseladb_range::mutate::Outcome, i: usize| {
        Document::from(teseladb_keys::stored::raw(&out.puts[i].value).unwrap())
    };
    let docs = vec![
        Document::new().with("_id", z.clone()).with("x", Document::new().with("t", z.clone())),
        Document::new().with("_id", 1).with("u", z.clone()).with("v", z.clone()),
    ];
    let b = Bound {
        mutation: DocMutation::Insert { docs, ordered: true, rules: Rules::default() },
        keys: vec![vec![1], vec![2]],
    };
    let out = b.eval(&[None, None], ts).unwrap();
    assert_eq!(stored(&out, 0).get("_id"), Some(&z));
    assert_eq!(
        stored(&out, 0).get("x"),
        Some(&Value::Document(Document::new().with("t", z.clone())))
    );
    assert_eq!(stored(&out, 1).get("u"), Some(&Value::Timestamp { time, increment: inc }));
    assert_eq!(stored(&out, 1).get("v"), Some(&Value::Timestamp { time, increment: inc + 1 }));
    let current = Document::new().with("_id", 1).with("t", z.clone()).to_bytes().unwrap();
    for (update, filled) in [
        (Document::new().with("t", z.clone()), true),
        (Document::new().with("$set", Document::new().with("t", z.clone())), false),
    ] {
        let b = Bound {
            mutation: DocMutation::Update {
                filter: Document::new(),
                update: Value::Document(update),
                array_filters: None,
                multi: false,
                upsert: None,
                image: Image::None,
                rules: Rules::default(),
            },
            keys: vec![vec![1]],
        };
        let out = b.eval(&[Some((1, &current))], ts).unwrap();
        let docs = out.puts.iter().filter(|p| p.value_type != teseladb_range::HISTORY_VALUE);
        assert_eq!(docs.count(), usize::from(filled));
        if filled {
            assert_eq!(stored(&out, 0).get("t"), Some(&Value::Timestamp { time, increment: inc }));
        }
    }
}

/// `warn` and `errorAndLog` write their lines (20294 and 7488700) with the namespace, the document
/// and its `errInfo`, as a `refrouter` 9.0.4's shard does (006 T052).
#[test]
fn validation_actions_write_their_log_lines() {
    let mut m = Mem::new();
    let gt1 = d(&[("a", d(&[("$gt", 1.into())]))]);
    m.run("db", doc(&[("create", s("c")), ("validator", gt1), ("validationAction", s("warn"))]));
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            ("documents", Value::Array(vec![d(&[("_id", s("w1")), ("a", 0.into())])])),
        ]),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1));
    m.run("db", doc(&[("collMod", s("c")), ("validationAction", s("errorAndLog"))]));
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            ("documents", Value::Array(vec![d(&[("_id", s("e1")), ("a", 0.into())])])),
        ]),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(121), "{r:?}");
    let u = d(&[("q", d(&[("_id", s("w1"))])), ("u", d(&[("$set", d(&[("b", 1.into())]))]))]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", Value::Array(vec![u]))]));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(121), "{r:?}");
    let ids: Vec<(i32, &str)> = m.logged.iter().map(|(i, msg, _)| (*i, msg.as_str())).collect();
    assert_eq!(
        ids,
        vec![
            (20294, "Document would fail validation"),
            (7488700, "Document failed validation"),
            (7488700, "Document failed validation"),
        ]
    );
    let attr = &m.logged[2].2;
    assert_eq!(attr.get("namespace"), Some(&s("db.c")));
    assert_eq!(field(attr, "document.b"), &Value::Int32(1));
    assert_eq!(field(attr, "errInfo.failingDocumentId"), &s("w1"));
}

/// `redactClientLogData` (013 FR-027): the validation lines keep the namespace and the names and
/// shapes of the document and its `errInfo`, every value `"###"`; `setParameter` takes the value
/// as BSON truthiness, as RefDb Enterprise 9.0.4 does (measured 31-10-2026).
#[test]
fn redaction_hides_the_values_of_the_logged_documents() {
    let mut m = Mem::new();
    let set = |m: &mut Mem, v: Value| {
        m.run("admin", doc(&[("setParameter", 1.into()), ("redactClientLogData", v)]))
    };
    let get = |m: &mut Mem| {
        let r =
            m.run("admin", doc(&[("getParameter", 1.into()), ("redactClientLogData", 1.into())]));
        field(&r, "redactClientLogData").clone()
    };
    assert_eq!(get(&mut m), Value::Boolean(false));
    // Measured: each value and what it leaves.
    for (v, now) in [
        (Value::Int32(1), true),
        (Value::Int32(0), false),
        (Value::Double(1.5), true),
        (s("yes"), true),
        (Value::Null, false),
        (d(&[]), true),
    ] {
        let r = set(&mut m, v.clone());
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{v:?}: {r:?}");
        assert_eq!(get(&mut m), Value::Boolean(now), "{v:?}");
    }
    let r = m.run(
        "admin",
        doc(&[
            ("getParameter", d(&[("showDetails", true.into())])),
            ("redactClientLogData", 1.into()),
        ]),
    );
    assert_eq!(field(&r, "redactClientLogData.settableAtRuntime"), &Value::Boolean(true));
    assert_eq!(field(&r, "redactClientLogData.settableAtStartup"), &Value::Boolean(false));
    let gt1 = d(&[("a", d(&[("$gt", 1.into())]))]);
    m.run("db", doc(&[("create", s("c")), ("validator", gt1), ("validationAction", s("warn"))]));
    let secret = d(&[
        ("_id", s("w1")),
        ("a", s("secretvalue")),
        ("n", d(&[("x", 6.into())])),
        ("arr", Value::Array(vec![1.into(), s("t")])),
    ]);
    m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(vec![secret]))]));
    let (id, _, attr) = m.logged.last().unwrap();
    assert_eq!(*id, 20294);
    assert_eq!(attr.get("namespace"), Some(&s("db.c")));
    let hidden = s("###");
    assert_eq!(field(attr, "document._id"), &hidden);
    assert_eq!(field(attr, "document.a"), &hidden);
    assert_eq!(field(attr, "document.n.x"), &hidden);
    assert_eq!(field(attr, "document.arr"), &Value::Array(vec![hidden.clone(), hidden.clone()]));
    assert_eq!(field(attr, "errInfo.failingDocumentId"), &hidden);
    assert!(!format!("{attr:?}").contains("secretvalue"));
    // Off again: the values are written.
    set(&mut m, false.into());
    let plain = d(&[("_id", s("w2")), ("a", s("visible"))]);
    m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(vec![plain]))]));
    assert_eq!(field(&m.logged.last().unwrap().2, "document.a"), &s("visible"));
}

/// An update by operators writes the description of what it changed in each document (005
/// FR-008, 014 FR-009) with its versions; a replacement, a no-op and a delete write none.
#[test]
fn an_update_by_operators_writes_its_change_description() {
    use teseladb_changestream::update_desc::UpdateDescription;
    let ts = Hlc { physical: 5_000, logical: 0 };
    let current =
        |i: i32| Document::new().with("_id", i).with("a", 1).with("m", 1).to_bytes().unwrap();
    let (one, two) = (current(1), current(2));
    let update = |u: Document, multi| Bound {
        mutation: DocMutation::Update {
            filter: Document::new(),
            update: Value::Document(u),
            array_filters: None,
            multi,
            upsert: None,
            image: Image::None,
            rules: Rules::default(),
        },
        keys: vec![vec![1], vec![2]],
    };
    let ops = Document::new()
        .with("$set", Document::new().with("a", 2).with("b", 3))
        .with("$unset", Document::new().with("m", ""));
    let out = update(ops, true).eval(&[Some((1, &one)), Some((1, &two))], ts).unwrap();
    let all = UpdateDescription::decode_all(&out.change).unwrap();
    let keys: Vec<&[u8]> = all.iter().map(|(k, _)| k.as_slice()).collect();
    assert_eq!(keys, [&[1u8][..], &[2u8][..]]);
    let want = UpdateDescription {
        updated: vec![("a".into(), Value::Int32(2)), ("b".into(), Value::Int32(3))],
        removed: vec!["m".into()],
        ..UpdateDescription::default()
    };
    assert!(all.iter().all(|(_, d)| *d == want), "{all:?}");
    for u in
        [Document::new().with("x", 1), Document::new().with("$set", Document::new().with("a", 1))]
    {
        let out = update(u, true).eval(&[Some((1, &one)), Some((1, &two))], ts).unwrap();
        assert!(out.change.is_empty());
    }
}

/// Each document a write changes keeps, next to its version, what its event needs (014 FR-005,
/// FR-009): the description of an update by operators and its position in the write; the first
/// document of a write with nothing to describe keeps nothing.
#[test]
fn a_write_keeps_the_history_of_its_documents_next_to_them() {
    use teseladb_changestream::meta::{ChangeMeta, meta_key};
    use teseladb_changestream::update_desc::UpdateDescription;
    let ts = Hlc { physical: 5_000, logical: 0 };
    let key = |i: i32| {
        teseladb_keys::KeyBuilder::document(0, 7, teseladb_bson::RawValue::Int32(i))
            .as_bytes()
            .to_vec()
    };
    let history = |out: &teseladb_range::mutate::Outcome| -> Vec<(Vec<u8>, ChangeMeta)> {
        out.puts
            .iter()
            .filter(|p| p.value_type == teseladb_range::HISTORY_VALUE)
            .map(|p| (p.key.clone(), ChangeMeta::decode(&p.value).unwrap()))
            .collect()
    };
    let current = |i: i32| Document::new().with("_id", i).with("a", 1).to_bytes().unwrap();
    let (one, two) = (current(1), current(2));
    let update = Bound {
        mutation: DocMutation::Update {
            filter: Document::new(),
            update: Value::Document(Document::new().with("$set", Document::new().with("a", 2))),
            array_filters: None,
            multi: true,
            upsert: None,
            image: Image::None,
            rules: Rules::default(),
        },
        keys: vec![key(1), key(2)],
    };
    let out = update.eval(&[Some((1, &one)), Some((1, &two))], ts).unwrap();
    let desc = UpdateDescription {
        updated: vec![("a".into(), Value::Int32(2))],
        ..UpdateDescription::default()
    };
    let want = vec![
        (
            meta_key(&key(1), 0),
            ChangeMeta {
                txn: None,
                retry: None,
                op: 0,
                update: Some(desc.clone()),
                replace: false,
                delete: false,
                doc: None,
                moved: None,
            },
        ),
        (
            meta_key(&key(2), 1),
            ChangeMeta {
                txn: None,
                retry: None,
                op: 1,
                update: Some(desc),
                replace: false,
                delete: false,
                doc: None,
                moved: None,
            },
        ),
    ];
    assert_eq!(history(&out), want);
    let docs = vec![Document::new().with("_id", 1), Document::new().with("_id", 2)];
    let insert = Bound {
        mutation: DocMutation::Insert { docs, ordered: true, rules: Rules::default() },
        keys: vec![key(1), key(2)],
    };
    let out = insert.eval(&[None, None], ts).unwrap();
    assert_eq!(
        history(&out),
        vec![(
            meta_key(&key(2), 1),
            ChangeMeta {
                txn: None,
                retry: None,
                op: 1,
                update: None,
                replace: false,
                delete: false,
                doc: None,
                moved: None
            }
        )]
    );
    // A replacement says so.
    let replace = Bound {
        mutation: DocMutation::Update {
            filter: Document::new(),
            update: Value::Document(Document::new().with("b", 1)),
            array_filters: None,
            multi: false,
            upsert: None,
            image: Image::None,
            rules: Rules::default(),
        },
        keys: vec![key(1)],
    };
    let out = replace.eval(&[Some((1, &one))], ts).unwrap();
    let want = ChangeMeta {
        txn: None,
        retry: None,
        op: 0,
        update: None,
        replace: true,
        delete: false,
        doc: None,
        moved: None,
    };
    assert_eq!(history(&out), vec![(meta_key(&key(1), 0), want)]);
    let out = insert.eval(&[None, None], ts).unwrap();
    assert_eq!(
        history(&out),
        vec![(
            meta_key(&key(2), 1),
            ChangeMeta {
                txn: None,
                retry: None,
                op: 1,
                update: None,
                replace: false,
                delete: false,
                doc: None,
                moved: None
            }
        )]
    );
}

/// A field `listCollections` does not take is named after the command, `listCollections`, like
/// every other command (RefDb r9.0.4 list_collections.idl).
#[test]
fn an_unknown_field_of_list_collections_names_the_command() {
    use teseladb_wire::generic::check_unknown_fields;
    let unknown = |name: &str| {
        let cmd = doc(&[(name, 1.into()), ("zzUnknown", 1.into())]);
        let e = check_unknown_fields(&cmd, name).expect_err("an unknown field");
        (e.code.code, e.errmsg)
    };
    let msg = |s: &str| format!("BSON field '{s}.zzUnknown' is an unknown field.");
    // RefDb r9.0.4 list_collections.idl: `cpp_name: listCollections` (refrouter and refnode, 09-11-2026).
    assert_eq!(unknown("listCollections"), (40415, msg("listCollections")));
    assert_eq!(unknown("find"), (40415, msg("find")));
}

/// `$type: "number"` does not take `MinKey`, also when a sort reads the documents: the
/// `find` of the battery's `query-type` failed with `InternalError` "a matched document matches
/// no more" over `{a: MinKey}` (04-11-2026).
#[test]
fn type_number_leaves_min_key_out_of_a_sorted_find() {
    let mut m = Mem::new();
    let docs = Value::Array(vec![
        (d(&[("_id", 1.into()), ("a", 1.into())])),
        (d(&[("_id", 17.into()), ("a", Value::MinKey)])),
        (d(&[("_id", 18.into()), ("a", Value::MaxKey)])),
    ]);
    m.run("db", doc(&[("insert", s("c")), ("documents", docs)]));
    for sort in [None, Some(d(&[("_id", 1.into())]))] {
        let mut cmd = doc(&[
            ("find", s("c")),
            ("filter", d(&[("a", d(&[("$type", s("number"))]))])),
            ("projection", d(&[("_id", 1.into())])),
        ]);
        if let Some(sort) = sort.clone() {
            cmd.push("sort", sort);
        }
        let r = m.run("db", cmd);
        assert_eq!(
            field(&r, "cursor.firstBatch"),
            &Value::Array(vec![(d(&[("_id", 1.into())]))]),
            "{sort:?} {r:?}"
        );
    }
}

/// A filter 200 levels below the command, the most a command may nest, reads like any other
/// filter: a `refrouter` 9.0.4 answers an empty batch over a collection holding a document of 180
/// levels (measured on the native `refrouter` on 09-11-2026, scenario `bson-errors`).
#[test]
fn a_filter_as_deep_as_a_command_may_nest_is_read() {
    let mut m = Mem::new();
    let stored = d(&[("_id", 1.into()), ("a", nested(179))]);
    let r = m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(vec![stored]))]));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");

    // `filter` is the first level below the command; `{a: … {x: 1}}` takes the other 199.
    let r = m.run("db", doc(&[("find", s("c")), ("filter", nested(199)), ("limit", 1.into())]));

    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(Vec::new()));
    // The other reads and writes that take a filter as deep (the first level of a pipeline's
    // `$match` and of an update's `q` sits one or two levels further down, within the 200).
    let f = || nested(199);
    let shallower = || nested(197);
    for cmd in [
        doc(&[("count", s("c")), ("query", f())]),
        doc(&[("distinct", s("c")), ("key", s("a")), ("query", f())]),
        doc(&[
            ("delete", s("c")),
            ("deletes", Value::Array(vec![d(&[("q", shallower()), ("limit", 0.into())])])),
        ]),
        doc(&[
            ("update", s("c")),
            (
                "updates",
                Value::Array(vec![d(&[
                    ("q", shallower()),
                    ("u", d(&[("$set", d(&[("b", 1.into())]))])),
                ])]),
            ),
        ]),
        doc(&[
            ("aggregate", s("c")),
            ("pipeline", Value::Array(vec![d(&[("$match", shallower())])])),
            ("cursor", d(&[])),
        ]),
    ] {
        let r = m.run("db", cmd.clone());
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd:?} {r:?}");
    }
}

#[test]
fn an_id_may_hold_a_db_ref_and_no_other_dollar_name() {
    // Measured on a `refnode` 9.0.4 on 14-11-2026 (native battery, `crud-id-bulk`): a `$` name of
    // an `_id` is valid when it is part of a DBRef, `$ref` with a string followed by `$id`, then
    // optionally `$db` with a string, at any depth and with other fields after them.
    let id = |pairs: &[(&str, Value)]| d(&[("_id", d(pairs))]);
    let (r, i, db) = (("$ref", s("y")), ("$id", 1.into()), ("$db", s("d")));
    let cases: Vec<(Value, Option<&str>)> = vec![
        (id(&[r.clone(), i.clone()]), None),
        (id(&[r.clone(), i.clone(), db.clone()]), None),
        (id(&[r.clone(), i.clone(), db.clone(), ("x", 1.into())]), None),
        (id(&[r.clone(), i.clone(), ("x", 1.into())]), None),
        (id(&[("a", d(&[r.clone(), i.clone()]))]), None),
        (id(std::slice::from_ref(&i)), Some("$id")),
        (id(std::slice::from_ref(&r)), Some("$ref")),
        (id(&[("$ref", 1.into()), i.clone()]), Some("$ref")),
        (id(&[r.clone(), i.clone(), ("$db", 2.into())]), Some("$db")),
        (id(std::slice::from_ref(&db)), Some("$db")),
        (id(&[r.clone(), ("x", 1.into()), i.clone()]), Some("$ref")),
        (id(&[("a", d(&[("$x", 1.into())]))]), Some("$x")),
        (id(&[r.clone(), i.clone(), ("$x", 1.into())]), Some("$x")),
    ];
    let mut m = Mem::new();
    for (n, (document, refused)) in cases.into_iter().enumerate() {
        let mut document = match document {
            Value::Document(d) => d,
            other => panic!("{other:?}"),
        };
        if let Some(Value::Document(inner)) = document.get_mut("_id") {
            inner.push("n", n as i32);
        }
        let docs = Value::Array(vec![Value::Document(document)]);
        let r = m.run("db", doc(&[("insert", s("refs")), ("documents", docs)]));
        match refused {
            None => assert_eq!(field(&r, "n"), &Value::Int32(1), "case {n}: {r:?}"),
            Some(name) => assert_eq!(
                field(&r, "writeErrors.0.errmsg"),
                &s(&format!(
                    "_id fields may not contain '$'-prefixed fields: {name} is not valid for storage."
                )),
                "case {n}"
            ),
        }
    }
}

#[test]
fn a_server_bulk_write_answers_its_failed_updates_and_refuses_no_operations() {
    // Measured on a `refrouter` 9.0.4 on 14-11-2026 (native battery, `crud-id-bulk`).
    let mut m = Mem::new();
    let ns = Value::Array(vec![d(&[("ns", s("db.a"))])]);
    let ops = Value::Array(vec![
        d(&[("insert", 0.into()), ("document", d(&[("_id", 1.into()), ("n", s("x"))]))]),
        d(&[
            ("update", 0.into()),
            ("filter", d(&[("_id", 1.into())])),
            ("updateMods", d(&[("$inc", d(&[("n", 1.into())]))])),
        ]),
    ]);
    let cmd = |ops: Value| doc(&[("bulkWrite", 1.into()), ("ops", ops), ("nsInfo", ns.clone())]);
    let r = m.run("admin", cmd(ops));
    // A failed update still says it modified nothing, after `n`.
    let failed = field(&r, "cursor.firstBatch.1");
    assert_eq!(field(failed.as_document().unwrap(), "code"), &Value::Int32(14), "{r:?}");
    let keys: Vec<&str> = failed.as_document().unwrap().iter().map(|(k, _)| k).collect();
    assert_eq!(&keys[keys.len() - 2..], ["n", "nModified"], "{failed:?}");
    assert_eq!(field(failed.as_document().unwrap(), "nModified"), &Value::Int32(0));
    // No operation at all is a batch of the wrong length.
    let r = m.run("admin", cmd(Value::Array(Vec::new())));
    assert_eq!(code(&r), 16, "{r:?}");
    let nodot = Value::Array(vec![d(&[("ns", s("nodot"))])]);
    let r = m.run(
        "admin",
        doc(&[("bulkWrite", 1.into()), ("ops", Value::Array(vec![])), ("nsInfo", nodot)]),
    );
    assert_eq!(code(&r), 16, "the length before the namespace: {r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &s("Write batch sizes must be between 1 and 100000. Got 0 operations.")
    );
}
