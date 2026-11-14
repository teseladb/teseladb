//! `listIndexes` with the `_id` index alone (secondary indexes come with 009) and the complete
//! collation a collection keeps, as the reference router 9.0.4 shows them (measured).

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

fn id_index() -> Document {
    doc(&[("v", 2.into()), ("key", d(&[("_id", 1.into())])), ("name", s("_id_"))])
}

#[test]
fn a_collection_lists_its_id_index() {
    let mut m = Mem::new();
    m.run("db", doc(&[("create", s("c"))]));
    let r = m.run("db", doc(&[("listIndexes", s("c"))]));
    assert_eq!(field(&r, "cursor.ns"), &s("db.c"));
    assert_eq!(field(&r, "cursor.id"), &Value::Int64(0));
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(vec![Value::Document(id_index())]));
    let r = m.run("db", doc(&[("listIndexes", s("c")), ("includeIndexBuildInfo", true.into())]));
    let spec = d(&[("spec", Value::Document(id_index()))]);
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(vec![spec]));
    // A first batch of 0 leaves a cursor open.
    let r = m.run("db", doc(&[("listIndexes", s("c")), ("cursor", d(&[("batchSize", 0.into())]))]));
    assert_ne!(field(&r, "cursor.id"), &Value::Int64(0));
    let r = m.run("db", doc(&[("listIndexes", s("none"))]));
    assert_eq!((code(&r), field(&r, "errmsg")), (26, &s("ns does not exist: db.none")));
    let r = m.run("db", doc(&[("listIndexes", 5.into())]));
    assert_eq!((code(&r), field(&r, "errmsg")), (73, &s("Collection name has invalid type int")));
    let r = m.run("db", doc(&[("listIndexes", s("c")), ("cursor", 5.into())]));
    assert_eq!(code(&r), 14);
}

#[test]
fn a_collection_keeps_its_complete_collation() {
    let mut m = Mem::new();
    m.run("db", doc(&[("create", s("f")), ("collation", d(&[("locale", s("fr"))]))]));
    m.run("db", doc(&[("create", s("p")), ("collation", d(&[("locale", s("simple"))]))]));
    let r = m.run("db", doc(&[("listIndexes", s("f"))]));
    let index = field(&r, "cursor.firstBatch.0");
    let Value::Document(index) = index else { panic!("{r:?}") };
    assert_eq!(field(index, "collation.locale"), &s("fr"));
    assert_eq!(field(index, "collation.strength"), &Value::Int32(3));
    assert_eq!(field(index, "collation.alternate"), &s("non-ignorable"));
    let r = m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("f"))]))]));
    assert_eq!(field(&r, "cursor.firstBatch.0.options.collation.caseFirst"), &s("off"));
    assert_eq!(field(&r, "cursor.firstBatch.0.idIndex.collation.backwards"), &false.into());
    // `simple` keeps no collation.
    let r = m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("p"))]))]));
    assert_eq!(field(&r, "cursor.firstBatch.0.options"), &d(&[]));
    // The same collation written in full is the same option; another one is refused, shown in
    // full without its version.
    let full = d(&[("locale", s("fr")), ("strength", 3.into())]);
    let r = m.run("db", doc(&[("create", s("f")), ("collation", full)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let other = d(&[("locale", s("fr")), ("strength", 2.into())]);
    let r = m.run("db", doc(&[("create", s("f")), ("collation", other)]));
    assert_eq!(code(&r), 48);
    let given = d(&[("locale", s("fr")), ("backwards", true.into())]);
    let back = m.run("db", doc(&[("create", s("f")), ("collation", given)]));
    let Value::String(text) = field(&back, "errmsg") else { panic!() };
    assert!(text.ends_with("normalization: false, backwards: true } }"), "{text}");
    let Value::String(msg) = field(&r, "errmsg") else { panic!() };
    assert!(msg.ends_with(
        "requested: { collation: { locale: \"fr\", caseLevel: false, caseFirst: \"off\", strength: 2, numericOrdering: false, alternate: \"non-ignorable\", maxVariable: \"punct\", normalization: false } }"
    ), "{msg}");
}

#[test]
fn create_checks_the_id_index_and_does_not_keep_it() {
    let mut m = Mem::new();
    let key = || d(&[("_id", 1.into())]);
    // What the reference restore tool sends is taken, and is not a collection option.
    let spec = d(&[("key", key()), ("ns", s("db.a")), ("name", s("_id_"))]);
    let r = m.run("db", doc(&[("create", s("a")), ("idIndex", spec)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let r = m.run("db", doc(&[("listCollections", 1.into())]));
    assert_eq!(field(&r, "cursor.firstBatch.0.options"), &d(&[]));
    let cases = [
        (
            5.into(),
            14,
            "BSON field 'create.idIndex' is the wrong type 'int', expected type 'object'",
        ),
        (
            d(&[("name", s("_id_"))]),
            9,
            "The 'key' field is a required property of an index specification",
        ),
        (
            d(&[("key", d(&[("_id", (-1).into())])), ("name", s("_id_"))]),
            2,
            "The field 'key' for an _id index must be {_id: 1}, but got { _id: -1 }",
        ),
        (
            d(&[("key", key()), ("name", s("_id_")), ("unique", true.into())]),
            197,
            "The field 'unique' is not valid for an _id index specification. Specification: { key: { _id: 1 }, name: \"_id_\", unique: true, v: 2 }",
        ),
        (
            d(&[("key", key()), ("name", s("_id_")), ("collation", d(&[("locale", s("fr"))]))]),
            2,
            "'idIndex' must have the same collation as the collection.",
        ),
        (
            d(&[("key", key()), ("collation", d(&[("locale", s("fr"))]))]),
            9,
            "The 'name' field is a required property of an index specification",
        ),
        (
            d(&[("key", key()), ("collation", 5.into())]),
            14,
            "The field 'collation' must be an object, but got int",
        ),
    ];
    for (i, (spec, c, msg)) in cases.into_iter().enumerate() {
        let r = m.run("db", doc(&[("create", s(&format!("b{i}"))), ("idIndex", spec)]));
        assert_eq!((code(&r), field(&r, "errmsg")), (c, &s(msg)));
    }
    // The same collation as the collection's, written short or `simple` without one, passes.
    let fr = || d(&[("locale", s("fr"))]);
    let spec = d(&[("key", key()), ("name", s("_id_")), ("collation", fr())]);
    let r = m.run("db", doc(&[("create", s("c")), ("collation", fr()), ("idIndex", spec)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    // Without a collation it takes the collection's, unless its version cannot have one.
    let spec = d(&[("key", key()), ("name", s("_id_"))]);
    let r = m.run("db", doc(&[("create", s("c4")), ("collation", fr()), ("idIndex", spec)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let spec = d(&[("key", key()), ("name", s("_id_")), ("v", 1.into())]);
    let r = m.run("db", doc(&[("create", s("c3")), ("collation", fr()), ("idIndex", spec)]));
    assert_eq!(code(&r), 2);
    let simple = d(&[("locale", s("simple"))]);
    let spec = d(&[("key", key()), ("name", s("_id_")), ("collation", simple), ("v", 2.into())]);
    let r = m.run("db", doc(&[("create", s("e")), ("idIndex", spec)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
}

#[test]
fn coll_mod_writes_the_level_and_action_of_validation() {
    let mut m = Mem::new();
    let options = |m: &mut Mem, name: &str| {
        let r =
            m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s(name))]))]));
        field(&r, "cursor.firstBatch.0.options").clone()
    };
    for (name, change) in [
        ("a", ("validator", d(&[("x", 1.into())]))),
        ("b", ("validationLevel", s("moderate"))),
        ("d", ("validator", d(&[]))),
    ] {
        m.run("db", doc(&[("create", s(name))]));
        m.run("db", doc(&[("collMod", s(name)), change]));
    }
    let both = |level: &str| [("validationLevel", s(level)), ("validationAction", s("error"))];
    let mut a = vec![("validator", d(&[("x", 1.into())]))];
    a.extend(both("strict"));
    assert_eq!(options(&mut m, "a"), d(&a));
    assert_eq!(options(&mut m, "b"), d(&both("moderate")));
    assert_eq!(options(&mut m, "d"), d(&both("strict")));
}

#[test]
fn constraint_needs_valid_documents_and_a_strict_level() {
    let mut m = Mem::new();
    let int = || d(&[("a", d(&[("$type", s("int"))]))]);
    let docs = Value::Array(vec![
        d(&[("_id", 1.into()), ("a", s("x"))]),
        d(&[("_id", 2.into()), ("a", 2.into())]),
    ]);
    m.run("db", doc(&[("insert", s("k")), ("documents", docs)]));
    m.run("db", doc(&[("collMod", s("k")), ("validator", int())]));
    let r = m.run("db", doc(&[("collMod", s("k")), ("validationLevel", s("constraint"))]));
    assert_eq!(code(&r), 12_370_902);
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "Cannot upgrade validationLevel to 'constraint': the collection contains documents that do not satisfy the validator. First offending document _id: 1 Run db.k.find({\"$nor\": [{ a: { $type: \"int\" } }]}) to find non-compliant documents."
        )
    );
    // Valid documents, but a level that was never `strict`.
    m.run("db", doc(&[("create", s("g")), ("validator", int())]));
    let r = m.run("db", doc(&[("collMod", s("g")), ("validationLevel", s("constraint"))]));
    assert_eq!(
        (code(&r), field(&r, "errmsg")),
        (
            2,
            &s(
                "Failed to set validation options :: caused by :: validationLevel can only be changed to 'constraint' from 'strict'"
            )
        )
    );
    m.run("db", doc(&[("collMod", s("g")), ("validationLevel", s("strict"))]));
    let r = m.run("db", doc(&[("collMod", s("g")), ("validationLevel", s("constraint"))]));
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "Failed to set validation options :: caused by :: Cannot upgrade validationLevel to 'constraint' without first setting prepareConstraintValidationLevel to true"
        )
    );
    m.run("db", doc(&[("collMod", s("g")), ("prepareConstraintValidationLevel", true.into())]));
    let listed = |m: &mut Mem| {
        let r =
            m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("g"))]))]));
        field(&r, "cursor.firstBatch.0.options").clone()
    };
    let Value::Document(o) = listed(&mut m) else { panic!() };
    let keys: Vec<&str> = o.iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        ["validator", "validationLevel", "validationAction", "prepareConstraintValidationLevel"]
    );
    let r = m.run("db", doc(&[("collMod", s("g")), ("validationLevel", s("constraint"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let Value::Document(o) = listed(&mut m) else { panic!() };
    assert_eq!(
        (o.get("validationLevel"), o.get("prepareConstraintValidationLevel")),
        (Some(&s("constraint")), None)
    );
    // At `constraint`, the validator stays and writes cannot bypass it.
    let r = m.run("db", doc(&[("collMod", s("g")), ("validator", d(&[]))]));
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "Failed to set validation options :: caused by :: Validator cannot be changed when Validation level is 'constraint'"
        )
    );
    let one = Value::Array(vec![d(&[("_id", 3.into()), ("a", 3.into())])]);
    let r = m.run(
        "db",
        doc(&[("insert", s("g")), ("documents", one), ("bypassDocumentValidation", true.into())]),
    );
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s("bypassDocumentValidation is not permitted with 'constraint' validationLevel")
    );
    // Preparing and then not upgrading leaves no flag once it is set to false.
    m.run("db", doc(&[("create", s("p"))]));
    m.run("db", doc(&[("collMod", s("p")), ("prepareConstraintValidationLevel", true.into())]));
    m.run("db", doc(&[("collMod", s("p")), ("prepareConstraintValidationLevel", false.into())]));
    let r = m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("p"))]))]));
    let both = d(&[("validationLevel", s("strict")), ("validationAction", s("error"))]);
    assert_eq!(field(&r, "cursor.firstBatch.0.options"), &both);
}

/// `create` with `idIndex` as the reference restore tool sends it for a collection dumped with a
/// version 1 `_id` index, and every check of the index specification, as the reference router 9.0.4
/// answers them (measured).
#[test]
fn create_keeps_the_version_of_the_id_index() {
    use teseladb_bson::Decimal128;
    let mut m = Mem::new();
    let key = || d(&[("_id", 1.into())]);
    let v1 = |v: Value| d(&[("key", key()), ("name", s("_id_")), ("v", v)]);
    let listed = |m: &mut Mem, name: &str| {
        let r =
            m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s(name))]))]));
        let id = field(&r, "cursor.firstBatch.0.idIndex").clone();
        let r = m.run("db", doc(&[("listIndexes", s(name))]));
        assert_eq!(field(&r, "cursor.firstBatch.0"), &id);
        (field(&r, "cursor.firstBatch.0.v").clone(), field(&r, "cursor.firstBatch.0.name").clone())
    };
    for (name, v, want) in [
        ("a", Value::Int32(1), 1),
        ("b", Value::Int64(1), 1),
        ("c", Value::Double(2.0), 2),
        ("e", Value::Decimal128("1".parse::<Decimal128>().expect("decimal")), 1),
    ] {
        let r = m.run("db", doc(&[("create", s(name)), ("idIndex", v1(v))]));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{name}");
        assert_eq!(listed(&mut m, name), (Value::Int32(want), s("_id_")), "{name}");
    }
    // Any name, and any `ns`, is taken; the index is still `_id_`, and the version is no
    // collection option: a second `create` with another version finds the same collection.
    let spec = d(&[("key", key()), ("name", s("x")), ("v", 1.into()), ("ns", 5.into())]);
    let r = m.run("db", doc(&[("create", s("f")), ("idIndex", spec)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    assert_eq!(listed(&mut m, "f"), (Value::Int32(1), s("_id_")));
    let r = m.run("db", doc(&[("create", s("a")), ("idIndex", v1(2.into()))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let r = m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("a"))]))]));
    assert_eq!(field(&r, "cursor.firstBatch.0.options"), &d(&[]));
    // `collMod` keeps it.
    m.run("db", doc(&[("collMod", s("a")), ("validator", d(&[("x", 1.into())]))]));
    assert_eq!(listed(&mut m, "a"), (Value::Int32(1), s("_id_")));
    let id = || ("key", key());
    let name = || ("name", s("_id_"));
    let cases: Vec<(Value, i32, &str)> = vec![
        (
            v1(3.into()),
            67,
            "Invalid index specification { key: { _id: 1 }, name: \"_id_\", v: 3 }; cannot create an index with v=3",
        ),
        (
            v1((-1).into()),
            67,
            "Invalid index specification { key: { _id: 1 }, name: \"_id_\", v: -1 }; cannot create an index with v=-1",
        ),
        (v1(s("x")), 14, "The field 'v' must be a number, but got string"),
        (v1(Value::Null), 14, "The field 'v' must be a number, but got null"),
        (v1(true.into()), 14, "The field 'v' must be a number, but got bool"),
        (v1(1.5.into()), 2, "Index version must be representable as a 32-bit integer, but got 1.5"),
        (
            v1(Value::Int64(1 << 40)),
            2,
            "Index version must be representable as a 32-bit integer, but got 1099511627776",
        ),
        (d(&[id()]), 9, "The 'name' field is a required property of an index specification"),
        (
            d(&[("key", d(&[("a", 1.into())]))]),
            9,
            "The 'name' field is a required property of an index specification",
        ),
        (
            d(&[name(), ("zz", 1.into())]),
            197,
            "The field 'zz' is not valid for an index specification. Specification: { name: \"_id_\", zz: 1 }",
        ),
        (
            d(&[id(), ("v", 3.into())]),
            67,
            "Invalid index specification { key: { _id: 1 }, v: 3 }; cannot create an index with v=3",
        ),
        (
            d(&[id(), name(), ("v", s("x")), ("zz", 1.into())]),
            197,
            "The field 'zz' is not valid for an index specification. Specification: { key: { _id: 1 }, name: \"_id_\", v: \"x\", zz: 1 }",
        ),
        (
            d(&[id(), ("name", 5.into()), ("v", 1.into())]),
            14,
            "The field 'name' must be a string, but got int",
        ),
        (d(&[("key", 5.into()), name()]), 14, "The field 'key' must be an object, but got int"),
        (
            d(&[id(), name(), ("v", 1.into()), ("collation", d(&[("locale", s("fr"))]))]),
            67,
            "Invalid index specification { key: { _id: 1 }, name: \"_id_\", v: 1, collation: { locale: \"fr\" } }; cannot create an index with the 'collation' option and v=1",
        ),
        (
            d(&[id(), name(), ("collation", d(&[]))]),
            2,
            "The field 'collation' cannot be an empty object.",
        ),
        (
            d(&[id(), name(), ("v", 1.into()), ("background", true.into())]),
            197,
            "The field 'background' is not valid for an _id index specification. Specification: { key: { _id: 1 }, name: \"_id_\", v: 1, background: true }",
        ),
        (
            d(&[id(), name(), ("hidden", false.into())]),
            197,
            "The field 'hidden' is not valid for an _id index specification. Specification: { key: { _id: 1 }, name: \"_id_\", hidden: false, v: 2 }",
        ),
        (
            d(&[id(), name(), ("hidden", 1.into()), ("v", 3.into())]),
            14,
            "The field 'hidden' must be a bool, but got int",
        ),
        (
            d(&[id(), name(), ("partialFilterExpression", 1.into())]),
            14,
            "The field 'partialFilterExpression' must be an object, but got int",
        ),
        (
            d(&[id(), name(), ("default_language", 1.into())]),
            14,
            "The field 'default_language' must be a string, but got int",
        ),
        (
            d(&[id(), name(), ("originalSpec", 1.into())]),
            14,
            "The field 'originalSpec' must be an object, but got int",
        ),
        (
            d(&[id(), name(), ("weights", d(&[("a", 1.into())]))]),
            67,
            "Invalid index specification { key: { _id: 1 }, name: \"_id_\", weights: { a: 1 } }; the field 'weights' can only be specified with text indexes",
        ),
        (
            d(&[id(), name(), ("wildcardProjection", 1.into())]),
            2,
            "The field 'wildcardProjection' is only allowed in 'wildcard' indexes",
        ),
        (
            d(&[
                ("key", d(&[("$**", 1.into())])),
                name(),
                ("wildcardProjection", d(&[("a", 1.into())])),
            ]),
            197,
            "The field 'wildcardProjection' is not valid for an _id index specification. Specification: { key: { $**: 1 }, name: \"_id_\", wildcardProjection: { a: 1 }, v: 2 }",
        ),
        (
            d(&[id(), name(), ("clustered", 1.into())]),
            67,
            "The 'unique' field is required when 'clustered' is specified",
        ),
        (
            d(&[id(), name(), ("clustered", true.into()), ("unique", true.into())]),
            6243700,
            "Error in specification { key: { _id: 1 }, name: \"_id_\", clustered: true, unique: true, v: 2 } :: caused by :: Cannot create index with option 'clustered' that does not match an existing clustered index",
        ),
        (d(&[("key", d(&[])), name()]), 67, "Index keys cannot be empty."),
        (d(&[("key", d(&[("a", s("bad"))])), name()]), 67, "Unknown index plugin 'bad'"),
        (
            d(&[id(), ("name", s(""))]),
            67,
            "Error in specification { key: { _id: 1 }, name: \"\", v: 2 } :: caused by :: index name cannot be empty",
        ),
    ];
    for (i, (spec, c, msg)) in cases.into_iter().enumerate() {
        let r = m.run("db", doc(&[("create", s(&format!("x{i}"))), ("idIndex", spec)]));
        assert_eq!((code(&r), field(&r, "errmsg")), (c, &s(msg)), "case {i}");
    }
}
