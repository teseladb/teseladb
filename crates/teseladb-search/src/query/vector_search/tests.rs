//! `$vectorSearch` queries as the reference search server 1.70.6 reads them on a 9.0.4 replica set (measured
//! 01-11-2026, index `ve`: `v` vector of 3 dimensions with `euclidean`, filters `g` and `n`).

use teseladb_bson::{Document, Value};

use super::*;

fn floats(xs: &[f64]) -> Value {
    Value::Array(xs.iter().map(|x| Value::Double(*x)).collect())
}

/// A valid approximate query, changed by `edit`.
fn spec(edit: impl FnOnce(&mut Document)) -> Document {
    let mut d = Document::new()
        .with("index", "ve")
        .with("path", "v")
        .with("queryVector", floats(&[1.0, 0.2, 0.1]))
        .with("numCandidates", 10)
        .with("limit", 10);
    edit(&mut d);
    d
}

fn set(d: &mut Document, k: &str, v: impl Into<Value>) {
    d.remove(k);
    d.push(k, v.into());
}

fn err(d: Document) -> String {
    VectorQuery::parse(&d).expect_err("an error").0
}

fn definition() -> VectorDefinition {
    let field = |t: &str, p: &str| Value::Document(Document::new().with("type", t).with("path", p));
    let vector = Document::new()
        .with("type", "vector")
        .with("path", "v")
        .with("numDimensions", 3)
        .with("similarity", "euclidean");
    let def = Document::new().with(
        "fields",
        Value::Array(vec![Value::Document(vector), field("filter", "g"), field("filter", "n")]),
    );
    VectorDefinition::parse(&def).expect("valid")
}

fn filter(f: Document) -> Document {
    spec(|d| set(d, "filter", f))
}

fn op(field: &str, op: &str, v: impl Into<Value>) -> Document {
    Document::new().with(field, Document::new().with(op, v.into()))
}

#[test]
fn reads_a_valid_query() {
    let q = VectorQuery::parse(&spec(|_| {})).expect("valid");
    assert_eq!(q.index, "ve");
    assert_eq!(q.path, "v");
    assert!(!q.exact);
    assert_eq!((q.num_candidates, q.limit), (10, 10));
    assert_eq!(q.vector, vec![1.0, 0.2, 0.1]);
    assert_eq!(q.check(&definition()).expect("indexed").0, 3);
    let q = VectorQuery::parse(&spec(|d| {
        d.remove("numCandidates");
        set(d, "exact", true);
        set(d, "limit", Value::Int64(3_000_000));
    }))
    .expect("valid");
    assert!(q.exact);
    assert_eq!(q.limit, 3_000_000);
    // An integral double and a long are integers.
    let q = VectorQuery::parse(&spec(|d| {
        set(d, "limit", 2.0);
        set(d, "numCandidates", Value::Int64(5));
        set(d, "queryVector", Value::Array(vec![Value::Int64(1), Value::Int32(0), 0.5.into()]));
    }))
    .expect("valid");
    assert_eq!((q.num_candidates, q.limit), (5, 2));
}

#[test]
fn checks_fields_in_the_reference_order() {
    assert_eq!(err(spec(|d| drop(d.remove("index")))), "\"index\" is required");
    assert_eq!(err(spec(|d| set(d, "index", 5))), "\"index\" must be a string");
    assert_eq!(err(spec(|d| drop(d.remove("path")))), "\"path\" is required");
    assert_eq!(err(spec(|d| set(d, "path", 5))), "\"path\" must be a string");
    assert_eq!(err(spec(|d| set(d, "exact", "y"))), "\"exact\" must be a boolean");
    assert_eq!(
        err(spec(|d| set(d, "exact", true))),
        "\"numCandidates\" must be omitted when \"exact\" is set to true"
    );
    let no_nc = "numCandidates is required for approximate vector search";
    assert_eq!(err(spec(|d| drop(d.remove("numCandidates")))), no_nc);
    assert_eq!(
        err(spec(|d| {
            d.remove("numCandidates");
            set(d, "exact", false);
            set(d, "queryVector", "x");
        })),
        no_nc
    );
    let bounds = "\"numCandidates\" must be within bounds [1..10000]";
    for n in [Value::Int32(0), Value::Int32(-1), Value::Int32(10_001), Value::Double(20_000.0)] {
        assert_eq!(err(spec(|d| set(d, "numCandidates", n))), bounds);
    }
    assert_eq!(err(spec(|d| set(d, "numCandidates", 5.5))), "\"numCandidates\" must be an integer");
    assert_eq!(err(spec(|d| set(d, "numCandidates", "5"))), "\"numCandidates\" must be a integer");
    assert_eq!(err(spec(|d| drop(d.remove("limit")))), "\"limit\" is required");
    assert_eq!(err(spec(|d| set(d, "limit", 2.5))), "\"limit\" must be an integer");
    // Beyond 32 bits (`m88.js` of lane 3, the reference search server 1.70.6).
    let over = "\"numCandidates\" overflowed, must fit in a 32 bit integer";
    let under = "\"numCandidates\" underflowed, must fit in a 32 bit integer";
    for (n, want) in [
        (Value::Double(1e19), over),
        (Value::Double(f64::INFINITY), over),
        (Value::Double(3e9), over),
        (Value::Int64(3_000_000_000), over),
        (Value::Double(-1e19), under),
        (Value::Double(f64::NEG_INFINITY), under),
        (Value::Int64(-3_000_000_000), under),
        (Value::Double(f64::NAN), "\"numCandidates\" must be an integer"),
    ] {
        assert_eq!(err(spec(|d| set(d, "numCandidates", n))), want);
    }
    let limit = "\"limit\" overflowed, must fit in a 32 bit integer";
    assert_eq!(err(spec(|d| set(d, "limit", f64::INFINITY))), limit);
    assert_eq!(
        err(spec(|d| {
            d.remove("numCandidates");
            set(d, "exact", true);
            set(d, "limit", Value::Int64(3_000_000_000));
        })),
        "\"limit\" overflowed, must fit in a 32 bit integer"
    );
    let over = "limit should be less than or equal to numCandidates";
    assert_eq!(err(spec(|d| set(d, "numCandidates", 2))), over);
    let exactly = "Exactly one and only one of query and queryVector can be present";
    assert_eq!(err(spec(|d| drop(d.remove("queryVector")))), exactly);
    assert_eq!(err(spec(|d| set(d, "query", "x"))), exactly);
    assert_eq!(
        err(spec(|d| set(d, "queryVector", "x"))),
        "\"queryVector\" Unexpected type found when parsing vector"
    );
    assert_eq!(
        err(spec(|d| set(d, "queryVector", Value::Array(vec![1.0.into(), "a".into()])))),
        "Unsupported BsonValue type: STRING"
    );
    assert_eq!(
        err(spec(|d| set(d, "queryVector", floats(&[f64::NAN, 0.0])))),
        "'queryVector' contains a non-finite value (Infinity or NaN) at position [0]. All values in \"queryVector\" must be finite."
    );
    // The query vector before the filter.
    assert_eq!(
        err(spec(|d| {
            set(d, "queryVector", floats(&[f64::NAN]));
            set(d, "filter", op("n", "$in", Value::Array(vec![])));
        })),
        "'queryVector' contains a non-finite value (Infinity or NaN) at position [0]. All values in \"queryVector\" must be finite."
    );
}

#[test]
fn names_bson_types_as_the_reference() {
    let cases: [(Value, &str); 8] = [
        (Value::Document(Document::new()), "DOCUMENT"),
        (Value::Array(vec![]), "ARRAY"),
        (Value::Boolean(true), "BOOLEAN"),
        (Value::Null, "NULL"),
        (Value::ObjectId([0; 12]), "OBJECT_ID"),
        (Value::DateTime(0), "DATE_TIME"),
        (Value::Binary { subtype: 0, bytes: vec![] }, "BINARY"),
        (Value::MinKey, "MIN_KEY"),
    ];
    for (v, name) in cases {
        let got = err(spec(|d| set(d, "queryVector", Value::Array(vec![v]))));
        assert_eq!(got, format!("Unsupported BsonValue type: {name}"));
    }
}

#[test]
fn checks_the_filter_shape() {
    let types = "must be a boolean, objectId, number, string, date, uuid, or null";
    let sub = Document::new().with("x", 1);
    assert_eq!(err(filter(Document::new().with("g", sub.clone()))), format!("\"filter\" {types}"));
    let arr = Value::Array(vec!["a".into()]);
    assert_eq!(err(filter(Document::new().with("g", arr.clone()))), format!("\"filter\" {types}"));
    assert_eq!(
        err(filter(op("g", "$elemMatch", Document::new().with("$eq", "a")))),
        "\"filter.g\" at least one of [$gt, $gte, $lt, $lte, $eq, $ne, $in, $nin, $exists, $not] must be present"
    );
    assert_eq!(err(filter(op("n", "$eq", sub.clone()))), format!("\"filter.n.$eq\" {types}"));
    assert_eq!(err(filter(op("n", "$ne", arr))), format!("\"filter.n.$ne\" {types}"));
    assert_eq!(
        err(filter(op("n", "$gt", Value::Null))),
        "\"filter.n.$gt\" Type is not supported: NULL"
    );
    assert_eq!(err(filter(op("n", "$lt", sub.clone()))), format!("\"filter.n.$lt\" {types}"));
    assert_eq!(
        err(filter(op("n", "$in", Value::Array(vec![])))),
        "\"filter.n.$in\" cannot be empty"
    );
    assert_eq!(
        err(filter(op("n", "$nin", Value::Array(vec![])))),
        "\"filter.n.$nin\" cannot be empty"
    );
    assert_eq!(
        err(filter(op("n", "$in", Value::Array(vec![Value::Null])))),
        "\"filter.n.$in[0]\" value type cannot be null"
    );
    assert_eq!(
        err(filter(op("n", "$in", Value::Array(vec![Value::Document(sub)])))),
        format!("\"filter.n.$in[0]\" {types}")
    );
    assert_eq!(
        err(filter(op("n", "$in", Value::Array(vec![3.into(), "x".into()])))),
        "\"filter.n.$in\" must have elements of the same type"
    );
    assert_eq!(err(filter(op("n", "$in", 3))), "\"filter.n.$in\" must be an array");
    assert_eq!(err(filter(op("n", "$exists", 1))), "\"filter.n.$exists\" must be a boolean");
    let in_empty = op("n", "$in", Value::Array(vec![]));
    let or = Document::new().with("$or", Value::Array(vec![Value::Document(in_empty)]));
    assert_eq!(err(filter(or)), "\"filter[0].n.$in\" cannot be empty");
    let bad_or = Document::new().with("$or", 5);
    assert_eq!(err(filter(bad_or)), format!("\"filter\" {types}"));
    let bad_clause = Document::new().with("$and", Value::Array(vec![5.into()]));
    assert_eq!(err(filter(bad_clause)), format!("\"filter[0]\" {types}"));
    let not_eq = op("n", "$not", 3);
    assert!(err(filter(not_eq)).starts_with("\"filter.n\" at least one of"));
    let expr = op("$expr", "$eq", Value::Array(vec!["$n".into(), 1.into()]));
    assert_eq!(err(filter(expr)), format!("\"filter.$expr.$eq\" {types}"));
    assert_eq!(err(spec(|d| set(d, "filter", 5))), format!("\"filter\" {types}"));
}

#[test]
fn accepts_the_reference_filters() {
    let uuid = Value::Binary { subtype: 4, bytes: vec![0; 16] };
    let fine = [
        op("n", "$gt", true),
        op("n", "$gte", "a"),
        op("n", "$lte", Value::DateTime(0)),
        op("n", "$ne", Value::Null),
        op("n", "$eq", uuid),
        op("n", "$in", Value::Array(vec![1.into(), 2.5.into(), Value::Int64(3)])),
        op("n", "$exists", false),
        op("n", "$not", Document::new().with("$not", Document::new().with("$gt", 1))),
        Document::new().with("n", Document::new().with("$gt", 1).with("$lt", 4)),
        Document::new().with("g", Value::ObjectId([1; 12])).with("n", 3),
        Document::new().with(
            "$and",
            Value::Array(vec![Value::Document(
                Document::new()
                    .with("$or", Value::Array(vec![Value::Document(Document::new().with("n", 1))])),
            )]),
        ),
    ];
    for f in fine {
        VectorQuery::parse(&filter(f.clone())).unwrap_or_else(|e| panic!("{f:?}: {}", e.0));
    }
    let q = VectorQuery::parse(&filter(Document::new().with("n", 1).with("g", "a").with("n", 2)))
        .expect("valid");
    assert_eq!(q.filter_paths, vec!["n".to_owned(), "g".to_owned()]);
}

#[test]
fn checks_against_the_index() {
    let def = definition();
    let q = |edit: fn(&mut Document)| VectorQuery::parse(&spec(edit)).expect("valid");
    assert_eq!(
        q(|d| set(d, "path", "w")).check(&def).expect_err("not vector").0,
        "w is not indexed as vector"
    );
    assert_eq!(
        q(|d| set(d, "path", "g")).check(&def).expect_err("not vector").0,
        "g is not indexed as vector"
    );
    assert_eq!(
        q(|d| set(d, "queryVector", floats(&[1.0, 2.0]))).check(&def).expect_err("dims").0,
        "vector field is indexed with 3 dimensions but queried with 2"
    );
    assert_eq!(
        q(|d| set(d, "queryVector", Value::Array(vec![]))).check(&def).expect_err("dims").0,
        "vector field is indexed with 3 dimensions but queried with 0"
    );
    // The dimensions before the filter's paths.
    let both = |d: &mut Document| {
        set(d, "queryVector", floats(&[1.0]));
        set(d, "filter", Document::new().with("z", 1));
    };
    assert_eq!(
        q(both).check(&def).expect_err("dims").0,
        "vector field is indexed with 3 dimensions but queried with 1"
    );
    for path in ["z", "v", "g.x"] {
        let got = VectorQuery::parse(&filter(Document::new().with(path, 1)))
            .expect("valid")
            .check(&def)
            .expect_err("not a filter")
            .0;
        assert_eq!(got, format!("Path '{path}' needs to be indexed as filter"));
    }
    assert_eq!(not_vector("v").0, "v must be indexed as \"vector\".");
    assert_eq!(not_search("ve").0, "Cannot execute $search over vectorSearch index 've'");
}
