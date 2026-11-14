//! `createIndexes` and `collMod` of indexes as a `refrouter` 9.0.4 answers them (009 T011, T013,
//! scenario `index-commands`, measured): errors forwarded from the shard with `code` and
//! `codeName` first, «Error in specification» around the specification errors, the TTL and
//! hashed-unique refusals, the `_id` index options, the specifications as `listIndexes` shows
//! them, and `collMod` of `hidden` and `expireAfterSeconds`.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "t";

fn spec(key: Document, name: &str) -> Document {
    Document::new().with("key", key).with("name", name)
}

fn create(m: &mut Mem, coll: &str, specs: Vec<Document>) -> Document {
    let specs = Value::Array(specs.into_iter().map(Value::Document).collect());
    m.run(DB, Document::new().with("createIndexes", coll).with("indexes", specs))
}

fn names(d: &Document) -> Vec<String> {
    d.iter().map(|(k, _)| k.to_string()).collect()
}

fn errmsg(d: &Document) -> String {
    match d.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("no errmsg: {other:?} in {d:?}"),
    }
}

fn listed(m: &mut Mem, name: &str) -> Document {
    let r = m.run(DB, Document::new().with("listIndexes", "c"));
    let Value::Array(batch) = field(&r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
    batch
        .into_iter()
        .find_map(|i| match i {
            Value::Document(d) if d.get("name") == Some(&Value::from(name)) => Some(d),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} not listed in {r:?}"))
}

#[test]
fn errors_come_as_a_refrouter_forwards_them() {
    let mut m = Mem::new();
    // `createIndexes` puts the shard's own reply first, under `raw` (measured on 03-11-2026).
    let r = create(&mut m, "c", vec![]);
    assert_eq!(names(&r)[..5], ["raw", "code", "codeName", "ok", "errmsg"]);
    // A collection missing from a database that exists is the shard's error; a database that
    // does not exist is the router's, as any error (battery's crud-database-entries, 04-11-2026).
    let drop =
        |m: &mut Mem| m.run(DB, Document::new().with("dropIndexes", "nope").with("index", "*"));
    assert_eq!(names(&drop(&mut m))[..4], ["ok", "errmsg", "code", "codeName"]);
    m.run(DB, Document::new().with("create", "c"));
    assert_eq!(names(&drop(&mut m))[..4], ["code", "codeName", "ok", "errmsg"]);
}

#[test]
fn specification_errors_name_the_specification() {
    let mut m = Mem::new();
    let cases = [
        (
            spec(Document::new(), "empty"),
            "Error in specification { key: {}, name: \"empty\" } :: caused by :: Index keys cannot be empty.",
        ),
        (
            spec(Document::new().with("a", "bogus"), "bogus"),
            "Error in specification { key: { a: \"bogus\" }, name: \"bogus\" } :: caused by :: Unknown index plugin 'bogus'",
        ),
        (
            Document::new().with("key", Document::new().with("a", 1)),
            "Error in specification { key: { a: 1 } } :: caused by :: The 'name' field is a required property of an index specification",
        ),
        (
            spec(Document::new().with("z", 1), "z").with("bogusOption", 1),
            "Error in specification { key: { z: 1 }, name: \"z\", bogusOption: 1 } :: caused by :: The field 'bogusOption' is not valid for an index specification. Specification: { key: { z: 1 }, name: \"z\", bogusOption: 1 }",
        ),
        (
            spec(Document::new().with("t", 1).with("a", 1), "t_a").with("expireAfterSeconds", 3600),
            "TTL indexes are single-field indexes, compound indexes do not support TTL. Index spec: { key: { t: 1, a: 1 }, name: \"t_a\", expireAfterSeconds: 3600 }",
        ),
        (
            spec(Document::new().with("_id", 1), "_id_").with("unique", false),
            "The field 'unique' is not valid for an _id index specification. Specification: { key: { _id: 1 }, name: \"_id_\", unique: false, v: 2 }",
        ),
    ];
    for (s, want) in cases {
        let r = create(&mut m, "c", vec![s]);
        assert_eq!(errmsg(&r), want);
    }
}

#[test]
fn a_hashed_index_cannot_be_unique() {
    let mut m = Mem::new();
    create(&mut m, "c", vec![spec(Document::new().with("a", "hashed"), "a_hashed")]);
    let s = spec(Document::new().with("a", "hashed"), "a_hashed_u").with("unique", true);
    let r = create(&mut m, "c", vec![s]);
    assert_eq!(field(&r, "code"), &Value::Int32(16764));
    let msg = errmsg(&r);
    assert!(msg.starts_with("Index build failed: "), "{msg}");
    assert!(
        msg.ends_with(
            "1 index specs provided. First index spec: { v: 2, unique: true, key: { a: \"hashed\" }, name: \"a_hashed_u\" } :: caused by :: Currently hashed indexes cannot guarantee uniqueness. Use a regular index."
        ),
        "{msg}"
    );
}

#[test]
fn indexes_are_listed_as_refdb_stores_them() {
    let mut m = Mem::new();
    let r = create(&mut m, "new", vec![spec(doc(&[("a", 1.into())]), "a_1")]);
    assert_eq!(field(&r, "createdCollectionAutomatically"), &Value::Boolean(false), "as a refrouter");
    let geo = spec(Document::new().with("loc", "2dsphere"), "loc_2dsphere");
    let plane = spec(Document::new().with("p", "2d"), "p_2d").with("bits", 20).with("min", -500);
    let plane = plane.with("max", 500);
    let es = Document::new().with("locale", "es").with("strength", 1);
    let coll = spec(Document::new().with("s", 1), "s_coll").with("collation", es);
    let text = spec(Document::new().with("s", "text"), "s_text");
    let r = create(&mut m, "c", vec![geo, plane, coll, text]);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(listed(&mut m, "loc_2dsphere").get("2dsphereIndexVersion"), Some(&Value::Int32(4)));
    let p = listed(&mut m, "p_2d");
    assert_eq!(
        (p.get("min"), p.get("max")),
        (Some(&Value::Double(-500.0)), Some(&Value::Double(500.0)))
    );
    let Some(Value::Document(c)) = listed(&mut m, "s_coll").get("collation").cloned() else {
        panic!("no collation")
    };
    assert_eq!(c.get("caseFirst"), Some(&Value::from("off")));
    assert_eq!(c.get("version"), Some(&Value::from("57.1")));
    let t = listed(&mut m, "s_text");
    assert_eq!(
        t,
        Document::new()
            .with("v", 2)
            .with("key", Document::new().with("_fts", "text").with("_ftsx", 1))
            .with("name", "s_text")
            .with("weights", Document::new().with("s", 1))
            .with("default_language", "english")
            .with("language_override", "language")
            .with("textIndexVersion", 3)
    );
    // The same text index again is already there.
    let again = create(&mut m, "c", vec![spec(Document::new().with("s", "text"), "s_text")]);
    assert_eq!(field(&again, "note"), &Value::from("all indexes already exist"));
}

#[test]
fn coll_mod_changes_hidden_and_the_ttl_of_an_index() {
    let mut m = Mem::new();
    let hidden = spec(Document::new().with("h", 1), "h_hidden").with("hidden", true);
    let ttl = spec(Document::new().with("t", 1), "t_ttl").with("expireAfterSeconds", 3600);
    create(&mut m, "c", vec![hidden, ttl]);
    let modify = |m: &mut Mem, index: Document| {
        m.run(DB, Document::new().with("collMod", "c").with("index", index))
    };
    let r = modify(&mut m, Document::new().with("name", "h_hidden").with("hidden", false));
    assert_eq!(
        r,
        Document::new().with("hidden_old", true).with("hidden_new", false).with("ok", 1.0)
    );
    assert_eq!(listed(&mut m, "h_hidden").get("hidden"), None, "unhidden drops the field");
    let by_key = Document::new().with("keyPattern", Document::new().with("t", 1));
    let r = modify(&mut m, by_key.with("expireAfterSeconds", 60));
    assert_eq!(
        r,
        Document::new()
            .with("expireAfterSeconds_old", 3600i64)
            .with("expireAfterSeconds_new", 60i64)
            .with("ok", 1.0)
    );
    assert_eq!(listed(&mut m, "t_ttl").get("expireAfterSeconds"), Some(&Value::Int32(60)));
    let r = modify(&mut m, Document::new().with("name", "nope").with("hidden", true));
    assert_eq!(field(&r, "code"), &Value::Int32(27));
    assert_eq!(errmsg(&r), "cannot find index nope for ns t.c");
}

/// The fields of the specification `listIndexes` shows for index `name`.
fn listed_fields(m: &mut Mem, name: &str) -> Vec<String> {
    names(&listed(m, name))
}

#[test]
fn specifications_are_stored_in_refdb_order_with_their_booleans() {
    let mut m = Mem::new();
    let fr = Document::new().with("locale", "fr");
    // Given in reverse, stored in the order of RefDb's specification (measured on 9.0.4).
    let reversed = spec(Document::new().with("o", 1), "o")
        .with("prepareUnique", true)
        .with("collation", fr.clone())
        .with(
            "storageEngine",
            Document::new().with(teseladb_names::STORAGE_ENGINE, Document::new()),
        )
        .with("expireAfterSeconds", 5)
        .with("sparse", 1)
        .with("background", 1)
        .with("hidden", true)
        .with("unique", false);
    let wildcard = spec(Document::new().with("$**", 1), "w")
        .with("prepareUnique", 1.5)
        .with("wildcardProjection", Document::new().with("a", 1))
        .with("collation", fr)
        .with("partialFilterExpression", Document::new().with("b", 1));
    let falsy = spec(Document::new().with("f", 1), "f")
        .with("unique", 0)
        .with("hidden", false)
        .with("prepareUnique", 0i64)
        .with("sparse", 0)
        .with("background", false);
    let truthy = spec(Document::new().with("u", 1), "u").with("sparse", true).with("unique", 1);
    let r = create(&mut m, "c", vec![reversed, wildcard, falsy, truthy]);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let o = listed(&mut m, "o");
    assert_eq!(
        names(&o),
        [
            "v",
            "key",
            "name",
            "background",
            "hidden",
            "sparse",
            "expireAfterSeconds",
            "storageEngine",
            "collation",
            "prepareUnique"
        ]
    );
    assert_eq!(
        (o.get("background"), o.get("sparse"), o.get("prepareUnique")),
        (Some(&Value::Boolean(true)), Some(&Value::Boolean(true)), Some(&Value::Boolean(true)))
    );
    assert_eq!(
        listed_fields(&mut m, "w"),
        [
            "v",
            "key",
            "name",
            "partialFilterExpression",
            "collation",
            "wildcardProjection",
            "prepareUnique"
        ]
    );
    let f = listed(&mut m, "f");
    assert_eq!(names(&f), ["v", "key", "name", "background", "sparse"]);
    assert_eq!(f.get("sparse"), Some(&Value::Boolean(false)));
    let u = listed(&mut m, "u");
    assert_eq!(names(&u), ["v", "key", "name", "unique", "sparse"]);
    assert_eq!(u.get("unique"), Some(&Value::Boolean(true)));
}

#[test]
fn boolean_options_must_be_booleans_or_numbers() {
    let mut m = Mem::new();
    for option in ["unique", "sparse", "prepareUnique", "background"] {
        let r = create(&mut m, "c", vec![spec(doc(&[("a", 1.into())]), "a").with(option, "x")]);
        assert_eq!(field(&r, "code"), &Value::Int32(14), "{option}: {r:?}");
        assert_eq!(
            errmsg(&r),
            format!(
                "Error in specification {{ key: {{ a: 1 }}, name: \"a\", {option}: \"x\" }} :: caused by :: The field '{option} has value {option}: \"x\", which is not convertible to bool"
            )
        );
    }
    let r = create(&mut m, "c", vec![spec(doc(&[("a", 1.into())]), "a").with("hidden", 1)]);
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { a: 1 }, name: \"a\", hidden: 1 } :: caused by :: The field 'hidden' must be a bool, but got int"
    );
}

#[test]
fn unique_and_prepare_unique_cannot_be_asked_together() {
    let mut m = Mem::new();
    let both = spec(doc(&[("a", 1.into())]), "a").with("unique", 1).with("prepareUnique", 1);
    let r = create(&mut m, "c", vec![both]);
    assert_eq!(field(&r, "code"), &Value::Int32(67));
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { a: 1 }, name: \"a\", unique: 1, prepareUnique: 1 } :: caused by :: Invalid index specification { key: { a: 1 }, name: \"a\", unique: 1, prepareUnique: 1 }; cannot create an index with the 'unique' option and the 'prepareUnique' option"
    );
    // `unique: 0` is no unique index: `prepareUnique` alone is kept.
    let r = create(
        &mut m,
        "c",
        vec![spec(doc(&[("b", 1.into())]), "b").with("unique", 0).with("prepareUnique", 1)],
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(listed(&mut m, "b").get("prepareUnique"), Some(&Value::Boolean(true)));
    let geo = spec(Document::new().with("g", "2dsphere"), "g")
        .with("2dsphereIndexVersion", 3)
        .with("weights", doc(&[("a", 1.into())]));
    let r = create(&mut m, "c", vec![geo]);
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { g: \"2dsphere\" }, name: \"g\", 2dsphereIndexVersion: 3, weights: { a: 1 } } :: caused by :: Invalid index specification { key: { g: \"2dsphere\" }, name: \"g\", 2dsphereIndexVersion: 3, weights: { a: 1 } }; the field 'weights' can only be specified with text indexes"
    );
    let id = spec(doc(&[("_id", 1.into())]), "_id_").with("prepareUnique", true);
    let r = create(&mut m, "c", vec![id]);
    assert_eq!(field(&r, "code"), &Value::Int32(197));
    assert_eq!(
        errmsg(&r),
        "The field 'prepareUnique' is not valid for an _id index specification. Specification: { key: { _id: 1 }, name: \"_id_\", prepareUnique: true, v: 2 }"
    );
}

#[test]
fn an_index_takes_hash_buckets_of_its_own() {
    // TeselaDB's own option (D-008, DV-008): an integer from 0 to 255, kept and listed.
    let mut m = Mem::new();
    let r =
        create(&mut m, "c", vec![spec(doc(&[("a", 1.into())]), "a").with("teselaBuckets", 4.0)]);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(listed(&mut m, "a").get("teselaBuckets"), Some(&Value::Double(4.0)));
    assert_eq!(m.indexes.indexes.values().next().map(|e| e.buckets), Some(4));
    for bad in [Value::Int32(256), Value::Double(1.5), Value::from("x"), Value::Int32(-1)] {
        let s = spec(doc(&[("b", 1.into())]), "b").with("teselaBuckets", bad.clone());
        let r = create(&mut m, "c", vec![s]);
        assert_eq!(field(&r, "code"), &Value::Int32(2), "{bad:?}: {r:?}");
        assert!(errmsg(&r).starts_with("teselaBuckets must be an integer from 0 to 255"), "{r:?}");
    }
}

#[test]
fn the_namespace_of_a_specification_is_not_kept() {
    // `refrestore` sends `ns` in the specifications of an old dump; a `refrouter` 9.0.4 drops it,
    // whatever namespace it names (measured on `lc12`).
    let mut m = Mem::new();
    let with_ns = |key: &str, ns: &str| spec(Document::new().with(key, 1), key).with("ns", ns);
    let r = create(&mut m, "c", vec![with_ns("a", "t.c"), with_ns("b", "other.c")]);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(listed_fields(&mut m, "a"), ["v", "key", "name"]);
    assert_eq!(listed_fields(&mut m, "b"), ["v", "key", "name"]);
}

#[test]
fn indexes_on_the_same_key_differ_by_their_identifying_options() {
    // Measured on a `refrouter` 9.0.4 (`lc12`): `unique`, `sparse`, `partialFilterExpression`,
    // `collation` and `wildcardProjection` make another index on the same key; `hidden`,
    // `expireAfterSeconds` and `prepareUnique` do not.
    let mut m = Mem::new();
    let s = || Document::new().with("s", 1);
    let ok = |r: &Document| r.get("ok") == Some(&Value::Double(1.0));
    assert!(ok(&create(&mut m, "c", vec![spec(s(), "s_1")])));
    let es = Document::new().with("locale", "es").with("strength", 1);
    let gt = |n: i32| Document::new().with("s", Document::new().with("$gt", n));
    for other in [
        spec(s(), "s_u").with("unique", true),
        spec(s(), "s_sp").with("sparse", true),
        spec(s(), "s_pf").with("partialFilterExpression", gt(1)),
        spec(s(), "s_co").with("collation", es.clone()),
        spec(s(), "s_pf2").with("partialFilterExpression", gt(2)),
    ] {
        let r = create(&mut m, "c", vec![other]);
        assert!(ok(&r), "{r:?}");
    }
    let all = Document::new().with("$**", 1);
    assert!(ok(&create(&mut m, "c", vec![spec(all.clone(), "all")])));
    let projected = spec(all, "proj").with("wildcardProjection", Document::new().with("a", 1));
    assert!(ok(&create(&mut m, "c", vec![projected])));
    let r = create(&mut m, "c", vec![spec(s(), "s_hid").with("hidden", true)]);
    assert_eq!(field(&r, "code"), &Value::Int32(85));
    assert_eq!(
        errmsg(&r),
        "An equivalent index already exists with a different name and options. Requested index: \
         { v: 2, hidden: true, key: { s: 1 }, name: \"s_hid\" }, existing index: { v: 2, key: \
         { s: 1 }, name: \"s_1\" }"
    );
    let r = create(&mut m, "c", vec![spec(s(), "s_ttl").with("expireAfterSeconds", 5)]);
    assert_eq!(
        errmsg(&r),
        "An equivalent index already exists with a different name and options. Requested index: \
         { v: 2, key: { s: 1 }, name: \"s_ttl\", expireAfterSeconds: 5 }, existing index: { v: \
         2, key: { s: 1 }, name: \"s_1\" }"
    );
    let r = create(&mut m, "c", vec![spec(s(), "s_co2").with("collation", es)]);
    assert_eq!(field(&r, "code"), &Value::Int32(85));
    assert_eq!(errmsg(&r), "Index already exists with a different name: s_co");
}

#[test]
fn only_one_field_of_an_index_may_be_hashed() {
    let mut m = Mem::new();
    let key = Document::new().with("v", "hashed").with("w", "hashed");
    let r = create(&mut m, "c", vec![spec(key, "two")]);
    assert_eq!(field(&r, "code"), &Value::Int32(31303), "{r:?}");
    let msg = errmsg(&r);
    assert!(msg.starts_with("Index build failed: "), "{msg}");
    assert!(
        msg.ends_with(
            "1 index specs provided. First index spec: { v: 2, key: { v: \"hashed\", w: \
             \"hashed\" }, name: \"two\" } :: caused by :: A maximum of one index field is \
             allowed to be hashed but found 2 for 'key' { v: \"hashed\", w: \"hashed\" }"
        ),
        "{msg}"
    );
}

#[test]
fn wildcard_specifications_are_checked_before_the_build() {
    // Measured on a `refrouter` 9.0.4 (`lc12`), with another `$**` index on the collection.
    let mut m = Mem::new();
    let all = || Document::new().with("$**", 1);
    assert_eq!(create(&mut m, "c", vec![spec(all(), "all")]).get("ok"), Some(&Value::Double(1.0)));
    let mixed = Document::new().with("a", 1).with("b", 0);
    let r = create(&mut m, "c", vec![spec(all(), "mixed").with("wildcardProjection", mixed)]);
    assert_eq!(field(&r, "code"), &Value::Int32(31254), "{r:?}");
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { $**: 1 }, name: \"mixed\", wildcardProjection: { a: 1, \
         b: 0 } } :: caused by :: Failed to parse projection: wildcardProjection :: caused by :: \
         Cannot do exclusion on field b in inclusion projection"
    );
    let sub = spec(Document::new().with("b.$**", 1), "sub_proj")
        .with("wildcardProjection", Document::new().with("b.c", 1));
    let r = create(&mut m, "c", vec![sub]);
    assert_eq!(field(&r, "code"), &Value::Int32(9), "{r:?}");
    let compound = spec(Document::new().with("e", 1).with("$**", 1), "compound_bad");
    let r = create(&mut m, "c", vec![compound]);
    assert_eq!(field(&r, "code"), &Value::Int32(67), "{r:?}");
    let r = create(&mut m, "c", vec![spec(all(), "wu").with("unique", true)]);
    assert_eq!(field(&r, "code"), &Value::Int32(67), "{r:?}");
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { $**: 1 }, name: \"wu\", unique: true, v: 2 } :: caused \
         by :: Index type 'wildcard' does not support the 'unique' option"
    );
    let r = create(&mut m, "c", vec![spec(all(), "wttl").with("expireAfterSeconds", 5)]);
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { $**: 1 }, name: \"wttl\", expireAfterSeconds: 5, v: 2 } \
         :: caused by :: cannot make a TTL index :: caused by :: Index type 'wildcard' does not \
         support the 'expireAfterSeconds' option"
    );
}
