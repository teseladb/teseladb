//! `prepareUnique` and the conversion to `unique` of `collMod` (009 FR-004, FR-007, contract
//! `index-commands.md`), with the replies and errors measured on RefDb 9.0.4: a prepared index
//! keeps the duplicates it has and refuses new ones; the conversion lists the groups of
//! documents with the same values and, once there are none, leaves a unique index under the same
//! name.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_index::prepared::{guard_key, guards_of};
use teseladb_keys::{KeyBuilder, KeyParts, PrimaryKey};

const DB: &str = "t";
const NS: &str = "t.c";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn create(m: &mut Mem, spec: Document) -> Document {
    m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![spec])))
}

fn index(field: &str) -> Document {
    Document::new().with("key", doc(&[(field, 1.into())])).with("name", format!("{field}_1"))
}

fn insert(m: &mut Mem, docs: Vec<Document>) -> Document {
    let cmd = Document::new().with("insert", "c").with("documents", arr(docs));
    m.run(DB, cmd.with("ordered", false))
}

fn one(id: i32, field: &str, v: i32) -> Document {
    doc(&[("_id", id.into()), (field, v.into())])
}

fn coll_mod(m: &mut Mem, index: Document) -> Document {
    m.run(DB, Document::new().with("collMod", "c").with("index", index))
}

fn named(name: &str) -> Document {
    Document::new().with("name", name)
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

/// `(code, errmsg)` of a failed reply.
fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("not an error: {r:?}"),
    }
}

/// The codes of the write errors of a reply.
fn write_codes(r: &Document) -> Vec<i32> {
    let Some(Value::Array(items)) = r.get("writeErrors") else { return Vec::new() };
    items
        .iter()
        .map(|e| match e {
            Value::Document(e) => match e.get("code") {
                Some(Value::Int32(c)) => *c,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
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

fn fields(d: &Document) -> Vec<String> {
    d.iter().map(|(k, _)| k.to_string()).collect()
}

fn violations(groups: Vec<Vec<i32>>) -> Value {
    Value::Array(
        groups
            .into_iter()
            .map(|ids| {
                let ids = Value::Array(ids.into_iter().map(Value::Int32).collect());
                Value::Document(Document::new().with("ids", ids))
            })
            .collect(),
    )
}

const UNPREPARED: &str =
    "Cannot make index unique with 'prepareUnique=false'. Run collMod to set it first.";
const CANNOT_CONVERT: &str = "Cannot convert the index to unique. Please resolve conflicting documents before running collMod again.";

#[test]
fn a_prepared_index_keeps_its_duplicates_and_refuses_new_ones() {
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, "a", 1), one(2, "a", 1), one(3, "a", 2)]);
    create(&mut m, index("a"));
    let r = coll_mod(&mut m, named("a_1").with("unique", true));
    assert_eq!(error(&r), (72, UNPREPARED.into()));
    let r = coll_mod(&mut m, named("a_1").with("prepareUnique", true));
    assert_eq!(
        r,
        Document::new()
            .with("prepareUnique_old", false)
            .with("prepareUnique_new", true)
            .with("ok", 1.0)
    );
    assert_eq!(fields(&listed(&mut m, "a_1")), ["v", "key", "name", "prepareUnique"]);
    // A value one document has, and a value two have, are both refused.
    let r = insert(&mut m, vec![one(4, "a", 2), one(5, "a", 1)]);
    assert_eq!(write_codes(&r), [11000, 11000], "{r:?}");
    let Some(Value::Array(errs)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(first) = &errs[0] else { panic!("{errs:?}") };
    assert_eq!(
        first.get("errmsg"),
        Some(&Value::from(
            "E11000 duplicate key error collection: t.c index: a_1 dup key: { a: 2 }"
        ))
    );
    assert_eq!(first.get("keyValue"), Some(&Value::Document(doc(&[("a", 2.into())]))));
    // A new value is taken once; two documents of one write cannot both take it.
    let r = insert(&mut m, vec![one(6, "a", 3), one(7, "a", 3)]);
    assert_eq!((field(&r, "n"), write_codes(&r)), (&Value::Int32(1), vec![11000]));
    // An update onto a held value is refused; one that keeps its value is not.
    let update = |m: &mut Mem, id: i32, set: Document| {
        let u = Document::new()
            .with("q", doc(&[("_id", id.into())]))
            .with("u", Document::new().with("$set", set));
        m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![u])))
    };
    assert_eq!(write_codes(&update(&mut m, 6, doc(&[("a", 2.into())]))), [11000]);
    let r = update(&mut m, 1, doc(&[("z", 1.into())]));
    assert_eq!((field(&r, "nModified"), write_codes(&r)), (&Value::Int32(1), vec![]));
    // Deleting one of two documents with a value leaves the other: the value stays held.
    let del = Document::new().with("q", doc(&[("_id", 2.into())])).with("limit", 1);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![del])));
    assert_eq!(write_codes(&insert(&mut m, vec![one(8, "a", 1)])), [11000]);
    // A value freed by a move is taken again.
    update(&mut m, 6, doc(&[("a", 9.into())]));
    assert_eq!(write_codes(&insert(&mut m, vec![one(9, "a", 3)])), Vec::<i32>::new());
}

#[test]
fn the_conversion_lists_the_duplicates_then_makes_the_index_unique() {
    let mut m = Mem::new();
    let docs = vec![
        one(30, "p", 6),
        one(31, "p", 5),
        one(32, "p", 6),
        one(33, "p", 5),
        one(34, "p", 6),
        one(35, "p", 7),
    ];
    insert(&mut m, docs);
    create(&mut m, index("p").with("hidden", true));
    coll_mod(&mut m, named("p_1").with("prepareUnique", true));
    let expected = violations(vec![vec![31, 33], vec![30, 32, 34]]);
    let r = coll_mod(&mut m, named("p_1").with("unique", true));
    assert_eq!(error(&r), (359, CANNOT_CONVERT.into()));
    assert_eq!(r.get("violations"), Some(&expected));
    let dry = Document::new()
        .with("collMod", "c")
        .with("index", named("p_1").with("unique", true))
        .with("dryRun", true);
    assert_eq!(m.run(DB, dry.clone()).get("violations"), Some(&expected));
    let del = |id: i32| Document::new().with("q", doc(&[("_id", id.into())])).with("limit", 1);
    let deletes = arr(vec![del(31), del(30), del(32)]);
    m.run(DB, Document::new().with("delete", "c").with("deletes", deletes));
    assert_eq!(m.run(DB, dry), ok());
    assert_eq!(fields(&listed(&mut m, "p_1")), ["v", "key", "name", "hidden", "prepareUnique"]);
    let r = coll_mod(&mut m, named("p_1").with("unique", true).with("hidden", false));
    assert_eq!(
        r,
        Document::new()
            .with("hidden_old", true)
            .with("hidden_new", false)
            .with("unique_new", true)
            .with("ok", 1.0)
    );
    assert_eq!(
        listed(&mut m, "p_1"),
        doc(&[
            ("v", 2.into()),
            ("key", Value::Document(doc(&[("p", 1.into())]))),
            ("name", "p_1".into()),
            ("unique", true.into()),
        ])
    );
    // One index under the name, ready, and unique entries only: no old entries, no guards.
    let indexes = m.indexes.indexes.values().filter(|e| e.collection != 0).count();
    assert_eq!(indexes, 1);
    let unique = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    for k in m.data.keys() {
        if let Ok(KeyParts::Data { index, .. }) = KeyParts::of(k) {
            assert!(index == 0 || index == unique, "a key of index {index} stayed");
        }
    }
    assert_eq!(write_codes(&insert(&mut m, vec![one(40, "p", 6)])), [11000]);
    // Already unique: nothing to do, also in a dry run; never back to non-unique.
    assert_eq!(coll_mod(&mut m, named("p_1").with("unique", true)), ok());
    assert_eq!(coll_mod(&mut m, named("p_1").with("prepareUnique", false)), ok());
    let r = coll_mod(&mut m, named("p_1").with("unique", false));
    assert_eq!(error(&r), (2, "Cannot make index non-unique".into()));
}

#[test]
fn an_index_built_prepared_takes_the_duplicates_already_there() {
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, "q", 1), one(2, "q", 1)]);
    let r = create(&mut m, index("q").with("prepareUnique", true));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(write_codes(&insert(&mut m, vec![one(3, "q", 1)])), [11000]);
    // The same index without the option conflicts with it.
    let r = create(&mut m, index("q"));
    assert_eq!(error(&r).0, 86, "{r:?}");
    // Dropping it drops its guards too.
    insert(&mut m, vec![one(4, "q", 4)]);
    let coll = m.catalog.collections.values().next().map(|c| c.id).unwrap_or(0);
    let id = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    let guards = KeyBuilder::index_prefix(0, coll, guards_of(id)).as_bytes().to_vec();
    assert!(m.data.keys().any(|k| k.starts_with(&guards)), "a guard was written");
    m.run(DB, Document::new().with("dropIndexes", "c").with("index", "q_1"));
    assert!(!m.data.keys().any(|k| k.starts_with(&guards)), "the guards went with the index");
}

#[test]
fn coll_mod_of_an_index_answers_its_errors_as_refdb() {
    let mut m = Mem::new();
    create(&mut m, index("a").with("expireAfterSeconds", 5));
    create(&mut m, index("b").with("hidden", true));
    let cases = [
        (named("a_1").with("foo", 1), 40415, "BSON field 'collMod.index.foo' is an unknown field."),
        (
            named("a_1").with("prepareUnique", "x"),
            14,
            "BSON field 'collMod.index.prepareUnique' is the wrong type 'string', expected types '[bool, double, decimal, int, long]'",
        ),
        (
            named("a_1").with("unique", "x"),
            14,
            "BSON field 'collMod.index.unique' is the wrong type 'string', expected types '[bool, double, decimal, int, long]'",
        ),
        (
            named("a_1").with("hidden", "x"),
            14,
            "BSON field 'collMod.index.hidden' is the wrong type 'string', expected types '[bool, double, decimal, int, long]'",
        ),
        (
            Document::new().with("keyPattern", 1).with("hidden", true),
            14,
            "BSON field 'collMod.index.keyPattern' is the wrong type 'int', expected type 'object'",
        ),
        (
            Document::new().with("name", 1).with("hidden", true),
            14,
            "BSON field 'collMod.index.name' is the wrong type 'int', expected type 'string'",
        ),
        (
            Document::new().with("prepareUnique", true),
            72,
            "Must specify either index name or key pattern.",
        ),
        (
            named("a_1").with("keyPattern", doc(&[("a", 1.into())])).with("hidden", true),
            72,
            "Cannot specify both key pattern and name.",
        ),
        (named("a_1"), 72, "no expireAfterSeconds, hidden, unique, or prepareUnique field"),
        (
            named("a_1").with("hidden", Value::Null),
            72,
            "no expireAfterSeconds, hidden, unique, or prepareUnique field",
        ),
        (named("nope").with("prepareUnique", true), 27, "cannot find index nope for ns t.c"),
        (
            named("a_1").with("prepareUnique", true).with("hidden", false),
            72,
            "prepareUnique cannot be combined with any other modification.",
        ),
        (
            named("a_1").with("prepareUnique", true).with("expireAfterSeconds", 5),
            72,
            "prepareUnique cannot be combined with any other modification.",
        ),
        (named("a_1").with("prepareUnique", true).with("unique", true), 72, UNPREPARED),
        (named("a_1").with("unique", 0), 2, "Cannot make index non-unique"),
    ];
    for (index, code, msg) in cases {
        let r = coll_mod(&mut m, index.clone());
        assert_eq!(error(&r), (code, msg.to_string()), "{index:?}");
    }
    // A collection option with an index change of `prepareUnique` is a combination too.
    let with_validator = Document::new()
        .with("collMod", "c")
        .with("index", named("b_1").with("prepareUnique", true))
        .with("validator", doc(&[("x", 1.into())]));
    let r = m.run(DB, with_validator);
    assert_eq!(error(&r).1, "prepareUnique cannot be combined with any other modification.");
    // A field that changes nothing answers nothing.
    assert_eq!(coll_mod(&mut m, named("b_1").with("hidden", 1)), ok());
    assert_eq!(coll_mod(&mut m, named("b_1").with("prepareUnique", false)), ok());
    assert_eq!(coll_mod(&mut m, named("a_1").with("expireAfterSeconds", 5)), ok());
    // `prepareUnique` that changes nothing does not count as a combination.
    coll_mod(&mut m, named("b_1").with("prepareUnique", true));
    let r = coll_mod(&mut m, named("b_1").with("prepareUnique", true).with("hidden", false));
    assert_eq!(
        r,
        Document::new().with("hidden_old", true).with("hidden_new", false).with("ok", 1.0)
    );
    // `hidden` goes after `name` whatever the order the options came in (measured).
    coll_mod(&mut m, named("b_1").with("hidden", true));
    assert_eq!(fields(&listed(&mut m, "b_1")), ["v", "key", "name", "hidden", "prepareUnique"]);
}

#[test]
fn dry_runs_only_check_a_conversion() {
    let mut m = Mem::new();
    create(&mut m, index("a"));
    let run = |m: &mut Mem, index: Option<Document>, dry: Value| {
        let mut cmd = Document::new().with("collMod", "c");
        if let Some(i) = index {
            cmd.push("index", i);
        }
        m.run(DB, cmd.with("dryRun", dry))
    };
    let r = run(&mut m, None, true.into());
    assert_eq!(error(&r), (72, "dry run mode requires an valid index modification.".into()));
    assert_eq!(run(&mut m, None, false.into()), ok());
    let r = run(&mut m, Some(named("a_1").with("unique", true)), "x".into());
    assert_eq!(
        error(&r),
        (
            14,
            "BSON field 'collMod.dryRun' is the wrong type 'string', expected types '[bool, double, decimal, int, long]'"
                .into()
        )
    );
    let r = run(&mut m, Some(named("a_1").with("hidden", true)), true.into());
    assert_eq!(error(&r).1, "dry run mode requires an index modification with unique: true.");
    let r = run(&mut m, Some(named("a_1").with("unique", true)), true.into());
    assert_eq!(error(&r).1, UNPREPARED);
    coll_mod(&mut m, named("a_1").with("prepareUnique", true));
    let r = run(&mut m, Some(named("a_1").with("unique", true).with("hidden", true)), true.into());
    assert_eq!(
        error(&r).1,
        "unique: true cannot be combined with any other modification in dry run mode."
    );
    let r = run(&mut m, Some(named("zz").with("unique", true)), true.into());
    assert_eq!(error(&r), (27, format!("cannot find index zz for ns {NS}")));
    // A dry run changes nothing.
    assert_eq!(run(&mut m, Some(named("a_1").with("unique", true)), 1.into()), ok());
    assert_eq!(listed(&mut m, "a_1").get("unique"), None);
}

#[test]
fn hashed_and_wildcard_indexes_are_prepared_but_never_unique() {
    let mut m = Mem::new();
    create(&mut m, Document::new().with("key", doc(&[("h", "hashed".into())])).with("name", "h"));
    create(&mut m, Document::new().with("key", doc(&[("$**", 1.into())])).with("name", "w"));
    for name in ["h", "w"] {
        let r = coll_mod(&mut m, named(name).with("prepareUnique", true));
        assert_eq!(field(&r, "prepareUnique_new"), &Value::Boolean(true), "{r:?}");
    }
    let r = coll_mod(&mut m, named("h").with("unique", true));
    assert_eq!(
        error(&r),
        (
            16764,
            "Currently hashed indexes cannot guarantee uniqueness. Use a regular index.".into()
        )
    );
    let r = coll_mod(&mut m, named("w").with("unique", true));
    assert_eq!(
        error(&r),
        (67, "Index type 'wildcard' does not support the 'unique' option".into())
    );
}

/// The names `listIndexes` gives, in its order.
fn names(m: &mut Mem) -> Vec<String> {
    let r = m.run(DB, Document::new().with("listIndexes", "c"));
    let Value::Array(batch) = field(&r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
    batch.iter().map(field_name).collect()
}

fn field_name(i: &Value) -> String {
    match i {
        Value::Document(d) => match d.get("name") {
            Some(Value::String(n)) => n.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// An index made unique keeps its place in `listIndexes`, also after a second conversion and in
/// a copy of the catalog taken from a snapshot (a `refrouter` 9.0.4 keeps it, measured).
#[test]
fn a_converted_index_keeps_its_place_in_the_list() {
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, "a", 1), one(2, "a", 2)]);
    for f in ["a", "b", "c"] {
        create(&mut m, index(f).with("prepareUnique", true));
    }
    assert_eq!(
        coll_mod(&mut m, named("a_1").with("unique", true)).get("unique_new"),
        Some(&true.into())
    );
    assert_eq!(names(&mut m), ["_id_", "a_1", "b_1", "c_1"]);
    coll_mod(&mut m, named("b_1").with("unique", true));
    assert_eq!(names(&mut m), ["_id_", "a_1", "b_1", "c_1"]);
    m.run(DB, Document::new().with("dropIndexes", "c").with("index", "a_1"));
    create(&mut m, index("d").with("prepareUnique", true));
    coll_mod(&mut m, named("d_1").with("unique", true));
    assert_eq!(names(&mut m), ["_id_", "b_1", "c_1", "d_1"]);
}

/// A hashed or wildcard index with duplicates answers them first, as a `refrouter` 9.0.4 does
/// (measured): the groups of a hashed index go in the order of the hashes, those of a wildcard
/// index by path and value.
#[test]
fn the_duplicates_come_before_the_type_of_the_index() {
    let mut m = Mem::new();
    let row =
        |id: i32, h: i32, w: i32| doc(&[("_id", id.into()), ("h", h.into()), ("w", w.into())]);
    insert(&mut m, vec![row(1, 1, 1), row(2, 1, 1), row(3, 2, 2), row(4, 2, 3), row(5, 2, 2)]);
    create(&mut m, Document::new().with("key", doc(&[("h", "hashed".into())])).with("name", "h"));
    create(&mut m, Document::new().with("key", doc(&[("$**", 1.into())])).with("name", "w"));
    for name in ["h", "w"] {
        coll_mod(&mut m, named(name).with("prepareUnique", true));
    }
    let r = coll_mod(&mut m, named("h").with("unique", true));
    assert_eq!(error(&r), (359, CANNOT_CONVERT.into()));
    assert_eq!(r.get("violations"), Some(&violations(vec![vec![3, 4, 5], vec![1, 2]])));
    let r = coll_mod(&mut m, named("w").with("unique", true));
    assert_eq!(error(&r), (359, CANNOT_CONVERT.into()));
    let groups = vec![vec![1, 2], vec![3, 4, 5], vec![1, 2], vec![3, 5]];
    assert_eq!(r.get("violations"), Some(&violations(groups)));
}

#[test]
fn the_duplicates_of_an_index_with_buckets_are_found_after_its_bucket() {
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, "b", 1), one(2, "b", 1), one(3, "b", 2), one(4, "b", 2)]);
    create(&mut m, index("b").with("teselaBuckets", 8).with("prepareUnique", true));
    let r = coll_mod(&mut m, named("b_1").with("unique", true));
    assert_eq!(r.get("violations"), Some(&violations(vec![vec![1, 2], vec![3, 4]])));
}

/// The guard keys of collection 1's index `id`: where a write keeps the versions of its values.
fn guard_range(m: &Mem, id: u32) -> Vec<u8> {
    let coll = m.catalog.collections.values().next().map(|c| c.id).unwrap_or(0);
    KeyBuilder::index_prefix(0, coll, guards_of(id)).as_bytes().to_vec()
}

/// Whether `start` is a prefix of a value of index `id` (a probe's read of the entries).
fn under_index(start: &[u8], id: u32) -> bool {
    matches!(KeyParts::of(start), Ok(KeyParts::Data { index, rest, .. }) if index == id && !rest.is_empty())
}

#[test]
fn a_write_probes_again_when_another_added_the_value_meanwhile() {
    let mut m = Mem::new();
    create(&mut m, index("a").with("prepareUnique", true));
    let id = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    // Another node adds the same value between this write's read of the guard and its read of
    // the entries: the guard moved, so the write probes again and sees that document's entry.
    let mut planted = false;
    m.on_scan_data = Some(Box::new(move |start, data| {
        if under_index(start, id) && !planted {
            planted = true;
            let guard = guard_key(start).expect("a prefix");
            data.insert(guard, 7u64.to_be_bytes().to_vec());
            let other =
                [start, PrimaryKey::id(teseladb_bson::RawValue::Int32(9)).as_bytes()].concat();
            data.insert(other, vec![]);
        }
    }));
    let r = insert(&mut m, vec![one(2, "a", 2)]);
    assert_eq!(write_codes(&r), [11000], "{r:?}");
}

#[test]
fn a_value_that_never_settles_is_a_write_conflict() {
    let mut m = Mem::new();
    create(&mut m, index("a").with("prepareUnique", true));
    let id = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    let mut version = 0u64;
    m.on_scan_data = Some(Box::new(move |start, data| {
        if under_index(start, id) {
            version += 1;
            data.insert(guard_key(start).expect("a prefix"), version.to_be_bytes().to_vec());
        }
    }));
    let r = insert(&mut m, vec![one(1, "a", 1)]);
    assert_eq!(write_codes(&r), [112], "{r:?}");
}

#[test]
fn a_guard_that_holds_no_version_fails_the_statement() {
    let mut m = Mem::new();
    create(&mut m, index("a").with("prepareUnique", true));
    insert(&mut m, vec![one(1, "a", 1)]);
    let id = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    let guards = guard_range(&m, id);
    let keys: Vec<Vec<u8>> = m.data.keys().filter(|k| k.starts_with(&guards)).cloned().collect();
    for k in keys {
        m.data.insert(k, vec![1]);
    }
    // The probe reads the guard and refuses it; the write reads it and fails its statement.
    let r = insert(&mut m, vec![one(2, "a", 1)]);
    assert_eq!(write_codes(&r), [1], "{r:?}");
}

#[test]
fn probes_travel_in_the_mutation_and_their_requests_in_the_error() {
    use teseladb_index::prepared::{Probe, ProbeRequest};
    use teseladb_wire::commands::index_maintenance::{
        probe_error, probe_requests, probes_value, read_probes,
    };
    use teseladb_wire::error::{CommandError, WRITE_CONFLICT};
    let probes = vec![
        Probe { guard: vec![1, 2], version: Some(u64::MAX), found: vec![vec![3]], limit: 2 },
        Probe { guard: vec![4], version: None, found: vec![], limit: 8 },
    ];
    let encoded = Document::new().with("p", probes_value(&probes)).to_bytes().expect("bytes");
    let raw = teseladb_bson::RawDoc::from_bytes(&encoded).expect("a document");
    assert_eq!(read_probes(raw.get("p")).expect("probes"), probes);
    assert_eq!(read_probes(None).expect("none"), vec![]);
    let bad = |v: Value| {
        let bytes = Document::new().with("p", v).to_bytes().expect("bytes");
        let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
        read_probes(raw.get("p")).is_err()
    };
    let item = |d: Document| Value::Array(vec![Value::Document(d)]);
    let bin = |b: &[u8]| Value::Binary { subtype: 0, bytes: b.to_vec() };
    let good = || Document::new().with("g", bin(&[1])).with("f", Value::Array(vec![]));
    assert!(bad(Value::Array(vec![Value::Int32(1)])), "an item that is no document");
    assert!(bad(item(good().with("v", 1).with("l", 2i64))), "a version of another type");
    assert!(bad(item(Document::new().with("g", bin(&[1])).with("l", 2i64))), "no found");
    assert!(bad(item(good().with("l", -1i64))), "a limit out of range");
    assert!(bad(item(good())), "no limit");
    assert!(bad(item(good().with("g", 1).with("l", 2i64))), "a guard that is no binary");
    let requests = vec![
        ProbeRequest { guard: vec![0xab, 1], prefix: vec![2], limit: 2 },
        ProbeRequest { guard: vec![3], prefix: vec![0xff], limit: 16 },
    ];
    let e = probe_error(&requests);
    assert_eq!(probe_requests(&e), Some(requests));
    for msg in [
        "prepared index probe: zz/01/2",
        "prepared index probe: 0/01/2",
        "prepared index probe: 01/01",
        "prepared index probe: 01/01/x",
        "prepared index probe: 01/01/2/3",
        "another conflict",
    ] {
        assert_eq!(probe_requests(&CommandError::new(WRITE_CONFLICT, msg)), None, "{msg}");
    }
    assert_eq!(probe_requests(&CommandError::new(teseladb_wire::error::BAD_VALUE, "x")), None);
}

#[test]
fn a_guard_spoiled_after_the_probe_fails_the_statement() {
    let mut m = Mem::new();
    create(&mut m, index("a").with("prepareUnique", true));
    let id = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    m.on_scan_data = Some(Box::new(move |start, data| {
        if under_index(start, id) {
            data.insert(guard_key(start).expect("a prefix"), vec![1]);
        }
    }));
    let r = insert(&mut m, vec![one(1, "a", 1)]);
    assert_eq!(write_codes(&r), [1], "{r:?}");
}

#[test]
fn a_duplicate_that_comes_during_the_conversion_fails_it() {
    use teseladb_bson::RawValue;
    use teseladb_keys::{Direction, FieldSpec, IndexKeySpec};
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, "p", 5)]);
    create(&mut m, index("p").with("prepareUnique", true));
    coll_mod(&mut m, named("p_1").with("prepareUnique", true));
    let coll = m.catalog.collections.values().next().map(|c| c.id).unwrap_or(0);
    let old = m.indexes.indexes.values().next().map(|e| e.id).unwrap_or(0);
    let spec =
        IndexKeySpec { fields: vec![FieldSpec { direction: Direction::Ascending }], buckets: 0 };
    // Another node's write lands while the unique index is built: a document with the same
    // value, with its entry in the old index.
    let mut planted = false;
    m.on_scan_data = Some(Box::new(move |start, data| {
        let documents = KeyBuilder::index_prefix(0, coll, 0).as_bytes().to_vec();
        if start.starts_with(&documents) && !planted {
            planted = true;
            let doc = doc(&[("_id", 2.into()), ("p", 5.into())]).to_bytes().expect("bytes");
            data.insert(KeyBuilder::document(0, coll, RawValue::Int32(2)).as_bytes().to_vec(), doc);
            let pk = PrimaryKey::id(RawValue::Int32(2));
            let (k, _) = KeyBuilder::index(0, coll, old, &spec, &[RawValue::Int32(5)], &pk, None);
            data.insert(k.as_bytes().to_vec(), vec![]);
        }
    }));
    let r = coll_mod(&mut m, named("p_1").with("unique", true));
    assert_eq!(error(&r).0, 359, "{r:?}");
    assert_eq!(r.get("violations"), Some(&violations(vec![vec![1, 2]])));
    assert_eq!(listed(&mut m, "p_1").get("prepareUnique"), Some(&Value::Boolean(true)));
    assert_eq!(m.indexes.indexes.len(), 1, "the unique index built went");
}

#[test]
fn more_index_changes_answer_as_refdb() {
    let mut m = Mem::new();
    create(&mut m, index("a").with("expireAfterSeconds", 5));
    create(&mut m, index("b"));
    let r = m.run(DB, Document::new().with("collMod", "c").with("index", 1));
    assert_eq!(
        error(&r),
        (14, "BSON field 'collMod.index' is the wrong type 'int', expected type 'object'".into())
    );
    let r = coll_mod(&mut m, named("b_1").with("expireAfterSeconds", 5));
    assert_eq!(error(&r), (72, "no expireAfterSeconds field to update".into()));
    let r = coll_mod(&mut m, named("a_1").with("expireAfterSeconds", "x"));
    assert_eq!(error(&r), (14, "expireAfterSeconds must be a number".into()));
    // `prepareUnique: false` takes the option away.
    coll_mod(&mut m, named("b_1").with("prepareUnique", true));
    let r = coll_mod(&mut m, named("b_1").with("prepareUnique", false));
    assert_eq!(
        r,
        Document::new()
            .with("prepareUnique_old", true)
            .with("prepareUnique_new", false)
            .with("ok", 1.0)
    );
    assert_eq!(listed(&mut m, "b_1").get("prepareUnique"), None);
    // A dry run takes no other change, of the index or of the collection.
    let dry = |m: &mut Mem, index: Document, extra: Option<(&str, Value)>| {
        let mut cmd = Document::new().with("collMod", "c").with("index", index);
        if let Some((k, v)) = extra {
            cmd.push(k, v);
        }
        m.run(DB, cmd.with("dryRun", true))
    };
    let combined = "unique: true cannot be combined with any other modification in dry run mode.";
    coll_mod(&mut m, named("a_1").with("prepareUnique", true));
    for (index, extra) in [
        (named("a_1").with("unique", true).with("expireAfterSeconds", 5), None),
        (named("a_1").with("unique", true).with("prepareUnique", true), None),
        (named("a_1").with("unique", true), Some(("validator", Value::Document(doc(&[]))))),
    ] {
        assert_eq!(error(&dry(&mut m, index, extra)).1, combined);
    }
    // Already unique: a dry run answers without a change.
    coll_mod(&mut m, named("a_1").with("unique", true));
    assert_eq!(dry(&mut m, named("a_1").with("unique", true), None), ok());
}

#[test]
fn a_text_index_becomes_unique_without_entries_to_check() {
    let mut m = Mem::new();
    insert(&mut m, vec![doc(&[("_id", 1.into()), ("t", "x".into())])]);
    let text = Document::new().with("key", doc(&[("t", "text".into())])).with("name", "t_text");
    create(&mut m, text);
    coll_mod(&mut m, named("t_text").with("prepareUnique", true));
    let r = coll_mod(&mut m, named("t_text").with("unique", true));
    assert_eq!(r, Document::new().with("unique_new", true).with("ok", 1.0));
}

#[test]
fn a_duplicate_of_an_index_with_a_collation_names_its_value() {
    let mut m = Mem::new();
    let fr = doc(&[("locale", "fr".into()), ("strength", 1.into())]);
    create(&mut m, index("s").with("collation", fr).with("prepareUnique", true));
    insert(&mut m, vec![doc(&[("_id", 1.into()), ("s", "Été".into())])]);
    let r = insert(&mut m, vec![doc(&[("_id", 2.into()), ("s", "ete".into())])]);
    assert_eq!(write_codes(&r), [11000], "{r:?}");
}

#[test]
fn a_conversion_fails_with_the_duplicate_when_the_old_entries_do_not_show_it() {
    use teseladb_bson::RawValue;
    let mut m = Mem::new();
    insert(&mut m, vec![one(1, "p", 5)]);
    create(&mut m, index("p").with("prepareUnique", true));
    let coll = m.catalog.collections.values().next().map(|c| c.id).unwrap_or(0);
    // A document written without the old index's entry: only the unique build sees it.
    let mut planted = false;
    m.on_scan_data = Some(Box::new(move |start, data| {
        let documents = KeyBuilder::index_prefix(0, coll, 0).as_bytes().to_vec();
        if start.starts_with(&documents) && !planted {
            planted = true;
            let d = doc(&[("_id", 2.into()), ("p", 5.into())]).to_bytes().expect("bytes");
            data.insert(KeyBuilder::document(0, coll, RawValue::Int32(2)).as_bytes().to_vec(), d);
        }
    }));
    let r = coll_mod(&mut m, named("p_1").with("unique", true));
    assert_eq!(error(&r).0, 11000, "{r:?}");
}
