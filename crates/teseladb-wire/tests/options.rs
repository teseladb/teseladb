//! Options and errors of the CRUD and catalog commands measured on a `refrouter` 9.0.4 (006 T024):
//! sort specifications, `hint`, `awaitData`, `$meta` the query cannot give, `update` with `sort`,
//! `listDatabases` outside `admin`, capped options, `constraint` with `warn`, the projection of
//! `findAndModify` read before the write, and the positional projection's errors.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn err(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("no error in {r:?}"),
    }
}

fn seeded() -> Mem {
    let mut m = Mem::new();
    let docs: Vec<Value> = (1..=4)
        .map(|i| {
            d(&[
                ("_id", i.into()),
                ("a", (i % 2).into()),
                ("b", Value::Array(vec![i.into(), (i * 10).into()])),
            ])
        })
        .collect();
    m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(docs))]));
    m
}

fn find(m: &mut Mem, extra: &[(&str, Value)]) -> Document {
    let mut c = doc(&[("find", s("c"))]);
    for (k, v) in extra {
        c.push(*k, v.clone());
    }
    m.run("db", c)
}

#[test]
fn sort_specifications() {
    let mut m = seeded();
    let cases: Vec<(Value, i32, &str)> = vec![
        (
            d(&[("a", d(&[("$meta", s("textScore"))]))]),
            40218,
            "query requires text score metadata, but it is not available",
        ),
        (
            d(&[("a", d(&[("$meta", s("searchScore"))]))]),
            40218,
            "query requires $search score metadata, but it is not available",
        ),
        (
            d(&[("a", d(&[("$meta", s("geoNearDistance"))]))]),
            40218,
            "query requires $geoNear distance metadata, but it is not available",
        ),
        (d(&[("a", d(&[("$meta", s("bogus"))]))]), 31138, "Illegal $meta sort: $meta: \"bogus\""),
        (d(&[("a", d(&[("$meta", 1.into())]))]), 31138, "Illegal $meta sort: $meta: 1"),
        (
            d(&[("a", d(&[("$meta", s("textScore")), ("x", 1.into())]))]),
            9,
            "Cannot have additional keys in a $meta sort specification",
        ),
        (d(&[("a", d(&[]))]), 17312, "$meta is the only expression supported by $sort right now"),
        (d(&[("a", s("x"))]), 15974, "Illegal key in $sort specification: a: \"x\""),
        (
            d(&[("a", 2.into())]),
            15975,
            "$sort key ordering must be 1 (for ascending) or -1 (for descending)",
        ),
        (d(&[("", 1.into())]), 40352, "FieldPath cannot be constructed with empty string"),
        (d(&[("a.", 1.into())]), 40353, "FieldPath must not end with a '.'."),
        (
            d(&[("a..b", 1.into())]),
            15998,
            "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not be empty strings.",
        ),
        (
            d(&[("$a", 1.into())]),
            16410,
            "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '$a'.",
        ),
        (1.into(), 14, "Expected field sort to be of type object"),
    ];
    for (sort, c, msg) in cases {
        let r = find(&mut m, &[("sort", sort.clone())]);
        assert_eq!(err(&r), (c, msg.to_owned()), "{sort:?}");
    }
    // `randVal` sorts in some order; `$text` makes `textScore` available.
    let r =
        find(&mut m, &[("sort", d(&[("a", d(&[("$meta", s("randVal"))])), ("_id", 1.into())]))]);
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(1));
    let text = d(&[("$text", d(&[("$search", s("x"))]))]);
    let r =
        find(&mut m, &[("filter", text), ("sort", d(&[("a", d(&[("$meta", s("textScore"))]))]))]);
    assert_eq!(err(&r).0, 27);
    let r = find(&mut m, &[("hint", 1.into())]);
    assert_eq!(err(&r), (9, "Hint must be a string or an object".to_owned()));
    let r = find(&mut m, &[("awaitData", true.into())]);
    assert_eq!(err(&r), (9, "Cannot set 'awaitData' without also setting 'tailable'".to_owned()));
    let r = find(&mut m, &[("projection", 1.into())]);
    assert_eq!(err(&r), (14, "Expected field projection to be of type object".to_owned()));
    let r = find(
        &mut m,
        &[
            ("projection", d(&[("k", d(&[("$meta", s("sortKey"))]))])),
            ("sort", d(&[("_id", (-1).into())])),
        ],
    );
    assert_eq!(field(&r, "cursor.firstBatch.0.k"), &Value::Array(vec![4.into()]));
    let r = find(&mut m, &[("projection", d(&[("k", d(&[("$meta", s("sortKey"))]))]))]);
    assert_eq!(err(&r).0, 2);
}

#[test]
fn positional_projection_errors_name_the_namespace() {
    let mut m = seeded();
    let r = find(
        &mut m,
        &[("filter", d(&[("_id", 1.into())])), ("projection", d(&[("b.$", 1.into())]))],
    );
    assert_eq!(
        err(&r),
        (51246, "Executor error during find command: db.c :: caused by :: positional operator '.$' couldn't find a matching element in the array".to_owned())
    );
    let r = find(
        &mut m,
        &[
            ("filter", d(&[("b", 40.into())])),
            ("projection", d(&[("b.$", 1.into())])),
            ("sort", d(&[("_id", 1.into())])),
        ],
    );
    assert_eq!(field(&r, "cursor.firstBatch.0.b"), &Value::Array(vec![40.into()]));
}

#[test]
fn update_with_sort() {
    let mut m = seeded();
    let u = |sort: Value, multi: bool| {
        d(&[
            ("q", d(&[("a", 1.into())])),
            ("u", d(&[("$set", d(&[("x", 1.into())]))])),
            ("sort", sort),
            ("multi", multi.into()),
        ])
    };
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            ("updates", Value::Array(vec![u(d(&[("_id", (-1).into())]), false)])),
        ]),
    );
    assert_eq!(field(&r, "nModified"), &Value::Int32(1));
    let r = find(&mut m, &[("filter", d(&[("x", 1.into())]))]);
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::Int32(3));
    let r = m.run(
        "db",
        doc(&[
            ("update", s("c")),
            ("updates", Value::Array(vec![u(d(&[("_id", 1.into())]), true)])),
        ]),
    );
    assert_eq!(field(&r, "writeErrors.0.errmsg"), &s("Cannot specify sort with multi=true"));
}

#[test]
fn catalog_options() {
    let mut m = seeded();
    let r = m.run("db", doc(&[("listDatabases", 1.into())]));
    assert_eq!(
        err(&r),
        (13, "listDatabases may only be run against the admin database.".to_owned())
    );
    let r = m.run("db", doc(&[("listDatabases", 1.into()), ("filter", 1.into())]));
    assert_eq!(err(&r).0, 14);
    let r = m.run("db", doc(&[("create", s("x")), ("capped", true.into())]));
    assert_eq!(err(&r), (72, "specify size:<n> when capped is true".to_owned()));
    for opt in ["size", "max"] {
        let r = m.run("db", doc(&[("create", s("x")), (opt, 10.into())]));
        assert_eq!(err(&r).0, 72, "{opt}");
    }
    let warn = "Validation action of 'warn' is not allowed when Validation level is 'constraint'";
    let r = m.run(
        "db",
        doc(&[
            ("create", s("v")),
            ("validator", d(&[("a", 1.into())])),
            ("validationLevel", s("constraint")),
            ("validationAction", s("warn")),
        ]),
    );
    assert_eq!(err(&r), (2, warn.to_owned()));
    m.run(
        "db",
        doc(&[
            ("create", s("v")),
            ("validator", d(&[("a", 1.into())])),
            ("validationLevel", s("constraint")),
        ]),
    );
    let r = m.run("db", doc(&[("collMod", s("v")), ("validationAction", s("warn"))]));
    assert_eq!(err(&r), (2, format!("Failed to set validation options :: caused by :: {warn}")));
    // The projection of findAndModify is read before the write.
    let r = m.run(
        "db",
        doc(&[
            ("findAndModify", s("c")),
            ("query", d(&[("_id", 1.into())])),
            ("update", d(&[("$set", d(&[("q", 1.into())]))])),
            ("fields", d(&[("a", 1.into()), ("b", 0.into())])),
        ]),
    );
    assert_eq!(err(&r).0, 31254);
    let r = find(&mut m, &[("filter", d(&[("q", 1.into())]))]);
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(vec![]));
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            ("documents", Value::Array(vec![d(&[("_id", d(&[("a", d(&[("$b", 1.into())]))]))])])),
        ]),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(52));
}
