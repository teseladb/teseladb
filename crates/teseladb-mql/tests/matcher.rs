//! Every query operator with arrays, embedded documents and mixed types, and the semantics of
//! dotted paths (006 T012, FR-009, FR-010).

pub mod common;

use common::{TestExt, bytes, raw};
use teseladb_collation::Collation;
use teseladb_mql::{ErrorCode, Filter, FilterUse, MatchContext, NoExtensions, codes};

fn matches_with(filter: &str, doc: &str, ctx: &MatchContext<'_>) -> bool {
    let f = bytes(filter);
    let d = bytes(doc);
    let parsed =
        Filter::parse(raw(&f), FilterUse::Query).unwrap_or_else(|e| panic!("{filter}: {e}"));
    parsed.matches(raw(&d), ctx).unwrap_or_else(|e| panic!("{filter} on {doc}: {e}"))
}

/// `(filter, document, matches)`.
fn check(cases: &[(&str, &str, bool)]) {
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    for &(filter, doc, want) in cases {
        assert_eq!(matches_with(filter, doc, &ctx), want, "{filter} on {doc}");
    }
}

fn parse_error(filter: &str, usage: FilterUse) -> (ErrorCode, String) {
    let f = bytes(filter);
    let e = Filter::parse(raw(&f), usage).expect_err(filter);
    (e.code, e.message)
}

#[test]
fn equality_paths_and_arrays() {
    check(&[
        (r#"{}"#, r#"{"a": 1}"#, true),
        (r#"{"a": 1}"#, r#"{"a": 1}"#, true),
        (r#"{"a": 1}"#, r#"{"a": 1.0}"#, true),
        (r#"{"a": 1}"#, r#"{"a": {"$numberDecimal": "1.00"}}"#, true),
        (r#"{"a": 1}"#, r#"{"a": "1"}"#, false),
        (r#"{"tags": "a"}"#, r#"{"tags": ["a", "b"]}"#, true),
        (r#"{"tags": ["a", "b"]}"#, r#"{"tags": ["a", "b"]}"#, true),
        (r#"{"tags": ["a", "b"]}"#, r#"{"tags": ["b", "a"]}"#, false),
        (r#"{"tags": ["a"]}"#, r#"{"tags": [["a"], "b"]}"#, true),
        (r#"{"tags": "a"}"#, r#"{"tags": [["a"]]}"#, false),
        (r#"{"a.b": 1}"#, r#"{"a": {"b": 1}}"#, true),
        (r#"{"a.b": 1}"#, r#"{"a": [{"b": 2}, {"b": 1}]}"#, true),
        (r#"{"a.b": 1}"#, r#"{"a": [{"b": [0, 1]}]}"#, true),
        (r#"{"a.b": 1}"#, r#"{"a": [[{"b": 1}]]}"#, false),
        (r#"{"a.1": 5}"#, r#"{"a": [4, 5]}"#, true),
        (r#"{"a.1": 5}"#, r#"{"a": [{"1": 5}]}"#, true),
        (r#"{"a.01": 5}"#, r#"{"a": [4, 5]}"#, false),
        (r#"{"a.0.b": 1}"#, r#"{"a": [{"b": 1}]}"#, true),
        (r#"{"a.0.1": 3}"#, r#"{"a": [[2, 3]]}"#, true),
        (r#"{"a.b.c": 1}"#, r#"{"a": [{"b": [{"c": 1}]}]}"#, true),
        (r#"{"a": {"b": 1}}"#, r#"{"a": {"b": 1}}"#, true),
        (r#"{"a": {"b": 1}}"#, r#"{"a": {"b": 1, "c": 2}}"#, false),
        (r#"{"a": {"b": 1, "c": 2}}"#, r#"{"a": {"c": 2, "b": 1}}"#, false),
        (r#"{"a": {"b": 1}}"#, r#"{"a": [{"b": 1}]}"#, true),
        (r#"{"a": null}"#, r#"{"b": 1}"#, true),
        (r#"{"a": null}"#, r#"{"a": null}"#, true),
        (r#"{"a": null}"#, r#"{"a": {"$undefined": true}}"#, false),
        (r#"{"a": null}"#, r#"{"a": [1, null]}"#, true),
        (r#"{"a": null}"#, r#"{"a": [1, 2]}"#, false),
        (r#"{"a.b": null}"#, r#"{"a": 5}"#, true),
        (r#"{"a.b": null}"#, r#"{"a": [{"b": 1}, {"c": 1}]}"#, true),
        (r#"{"a.b": null}"#, r#"{"a": [1, 2]}"#, false),
        (r#"{"a.b": null}"#, r#"{"a": []}"#, false),
        (r#"{"a": {"$eq": 1}}"#, r#"{"a": [1]}"#, true),
        (
            r#"{"a": {"$eq": {"$regularExpression": {"pattern": "x", "options": ""}}}}"#,
            r#"{"a": "x"}"#,
            false,
        ),
        (
            r#"{"a": {"$eq": {"$regularExpression": {"pattern": "x", "options": ""}}}}"#,
            r#"{"a": {"$regularExpression": {"pattern": "x", "options": ""}}}"#,
            true,
        ),
        (r#"{"a": {"$minKey": 1}}"#, r#"{"a": {"$minKey": 1}}"#, true),
        (r#"{"a": {"$numberDouble": "NaN"}}"#, r#"{"a": {"$numberDecimal": "NaN"}}"#, true),
        (r#"{"a": 1, "b": 2}"#, r#"{"a": 1, "b": 3}"#, false),
        (r#"{"$comment": "x", "a": 1}"#, r#"{"a": 1}"#, true),
    ]);
}

#[test]
fn comparisons_are_type_bracketed() {
    check(&[
        (r#"{"a": {"$gt": 1}}"#, r#"{"a": 2}"#, true),
        (r#"{"a": {"$gt": 1}}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$gte": 1}}"#, r#"{"a": 1}"#, true),
        (r#"{"a": {"$lt": 1}}"#, r#"{"a": 0.5}"#, true),
        (r#"{"a": {"$lte": 1}}"#, r#"{"a": {"$numberLong": "1"}}"#, true),
        (r#"{"a": {"$gt": 1}}"#, r#"{"a": "2"}"#, false),
        (r#"{"a": {"$lt": "b"}}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$gt": 1}}"#, r#"{"a": [0, 5]}"#, true),
        (r#"{"a": {"$gt": 1, "$lt": 3}}"#, r#"{"a": [0, 5]}"#, true),
        (r#"{"a": {"$gt": [1]}}"#, r#"{"a": [[2]]}"#, true),
        (r#"{"a": {"$gt": [1]}}"#, r#"{"a": [2]}"#, true),
        (r#"{"a": {"$gt": {"$minKey": 1}}}"#, r#"{"a": "x"}"#, true),
        (r#"{"a": {"$gt": {"$minKey": 1}}}"#, r#"{"a": {"$minKey": 1}}"#, false),
        (r#"{"a": {"$lt": {"$maxKey": 1}}}"#, r#"{"a": null}"#, true),
        (r#"{"a": {"$gt": {"$minKey": 1}}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$gte": null}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$lte": null}}"#, r#"{"a": null}"#, true),
        (r#"{"a": {"$gt": null}}"#, r#"{"b": 1}"#, false),
        (r#"{"a": {"$lt": null}}"#, r#"{"a": null}"#, false),
        (r#"{"a": {"$lt": 5}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, false),
        (r#"{"a": {"$gt": 5}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, false),
        (
            r#"{"a": {"$gte": {"$numberDouble": "NaN"}}}"#,
            r#"{"a": {"$numberDouble": "NaN"}}"#,
            true,
        ),
        (
            r#"{"a": {"$gt": {"$numberDouble": "NaN"}}}"#,
            r#"{"a": {"$numberDouble": "NaN"}}"#,
            false,
        ),
        (r#"{"a": {"$lte": {"$numberDouble": "NaN"}}}"#, r#"{"a": 1}"#, false),
        (
            r#"{"a": {"$lt": {"$date": "2026-01-01T00:00:00Z"}}}"#,
            r#"{"a": {"$date": "2025-01-01T00:00:00Z"}}"#,
            true,
        ),
        (r#"{"a": {"$gt": {"x": 1}}}"#, r#"{"a": {"x": 2}}"#, true),
        (r#"{"a": {"$ne": 1}}"#, r#"{"a": 2}"#, true),
        (r#"{"a": {"$ne": 1}}"#, r#"{"a": [1, 2]}"#, false),
        (r#"{"a": {"$ne": 1}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$ne": null}}"#, r#"{"b": 1}"#, false),
        (r#"{"a.b": {"$ne": null}}"#, r#"{"a": [{"b": 1}, {"c": 1}]}"#, false),
    ]);
}

#[test]
fn in_and_nin() {
    check(&[
        (r#"{"a": {"$in": [1, 2]}}"#, r#"{"a": 2}"#, true),
        (r#"{"a": {"$in": [1, 2]}}"#, r#"{"a": [5, 2]}"#, true),
        (r#"{"a": {"$in": []}}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$in": [null]}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$in": [[1, 2]]}}"#, r#"{"a": [1, 2]}"#, true),
        (
            r#"{"a": {"$in": [{"$regularExpression": {"pattern": "^x", "options": ""}}]}}"#,
            r#"{"a": "xy"}"#,
            true,
        ),
        (
            r#"{"a": {"$in": [{"$regularExpression": {"pattern": "^x", "options": ""}}]}}"#,
            r#"{"a": 3}"#,
            false,
        ),
        (r#"{"a": {"$nin": [1, 2]}}"#, r#"{"a": 3}"#, true),
        (r#"{"a": {"$nin": [1, 2]}}"#, r#"{"a": [3, 1]}"#, false),
        (r#"{"a": {"$nin": [1]}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$nin": [null]}}"#, r#"{"b": 1}"#, false),
    ]);
}

#[test]
fn logical_operators() {
    check(&[
        (r#"{"$and": [{"a": 1}, {"b": 2}]}"#, r#"{"a": 1, "b": 2}"#, true),
        (r#"{"$and": [{"a": 1}, {"b": 2}]}"#, r#"{"a": 1, "b": 3}"#, false),
        (r#"{"$or": [{"a": 1}, {"b": 2}]}"#, r#"{"a": 0, "b": 2}"#, true),
        (r#"{"$or": [{"a": 1}, {"b": 2}]}"#, r#"{"a": 0, "b": 0}"#, false),
        (r#"{"$nor": [{"a": 1}, {"b": 2}]}"#, r#"{"a": 0, "b": 0}"#, true),
        (r#"{"$nor": [{"a": 1}, {"b": 2}]}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$not": {"$gt": 5}}}"#, r#"{"a": 3}"#, true),
        (r#"{"a": {"$not": {"$gt": 5}}}"#, r#"{"b": 3}"#, true),
        (r#"{"a": {"$not": {"$gt": 5}}}"#, r#"{"a": [3, 7]}"#, false),
        (r#"{"a": {"$not": {"$gt": 1, "$lt": 5}}}"#, r#"{"a": 3}"#, false),
        (
            r#"{"a": {"$not": {"$regularExpression": {"pattern": "^x", "options": ""}}}}"#,
            r#"{"a": "y"}"#,
            true,
        ),
        (r#"{"a": {"$not": {"$regex": "^x", "$options": "i"}}}"#, r#"{"a": "X"}"#, false),
        (r#"{"$alwaysTrue": 1}"#, r#"{}"#, true),
        (r#"{"$alwaysFalse": 1}"#, r#"{}"#, false),
        (r#"{"$or": [{"$alwaysFalse": 1}, {"$and": [{"$alwaysTrue": 1}]}]}"#, r#"{}"#, true),
    ]);
}

#[test]
fn element_operators() {
    check(&[
        (r#"{"a": {"$exists": true}}"#, r#"{"a": null}"#, true),
        (r#"{"a": {"$exists": true}}"#, r#"{"b": 1}"#, false),
        (r#"{"a": {"$exists": false}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$exists": 0}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$exists": "x"}}"#, r#"{"a": 1}"#, true),
        (r#"{"a.b": {"$exists": true}}"#, r#"{"a": [{"c": 1}, {"b": 1}]}"#, true),
        (r#"{"a.b": {"$exists": true}}"#, r#"{"a": [1, 2]}"#, false),
        (r#"{"a.b": {"$exists": false}}"#, r#"{"a": [1, 2]}"#, true),
        (r#"{"a": {"$type": "string"}}"#, r#"{"a": "x"}"#, true),
        (r#"{"a": {"$type": 2}}"#, r#"{"a": "x"}"#, true),
        (r#"{"a": {"$type": 2.0}}"#, r#"{"a": "x"}"#, true),
        (r#"{"a": {"$type": "number"}}"#, r#"{"a": {"$numberDecimal": "1"}}"#, true),
        (r#"{"a": {"$type": ["string", "int"]}}"#, r#"{"a": 1}"#, true),
        (r#"{"a": {"$type": "array"}}"#, r#"{"a": []}"#, true),
        (r#"{"a": {"$type": "array"}}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$type": "string"}}"#, r#"{"a": [1, "x"]}"#, true),
        (r#"{"a": {"$type": "null"}}"#, r#"{"b": 1}"#, false),
        (r#"{"a": {"$type": -1}}"#, r#"{"a": {"$minKey": 1}}"#, true),
        (r#"{"a": {"$type": 127}}"#, r#"{"a": {"$maxKey": 1}}"#, true),
        (r#"{"a": {"$type": "long"}}"#, r#"{"a": {"$numberLong": "5"}}"#, true),
        (r#"{"a": {"$type": "object"}}"#, r#"{"a": {}}"#, true),
    ]);
}

#[test]
fn evaluation_operators() {
    check(&[
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": 8}"#, true),
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": 9}"#, false),
        (r#"{"a": {"$mod": [4.5, 0]}}"#, r#"{"a": 8}"#, true),
        (r#"{"a": {"$mod": [4, 1]}}"#, r#"{"a": 9.9}"#, true),
        (r#"{"a": {"$mod": [-4, -1]}}"#, r#"{"a": -9}"#, true),
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, false),
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": {"$numberDouble": "Infinity"}}"#, false),
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": {"$numberDecimal": "8.7"}}"#, true),
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": [1, 4]}"#, true),
        (r#"{"a": {"$mod": [-1, 0]}}"#, r#"{"a": {"$numberLong": "-9223372036854775808"}}"#, true),
        (r#"{"a": {"$mod": [4, 0]}}"#, r#"{"a": "8"}"#, false),
        (r#"{"a": {"$regex": "^ab"}}"#, r#"{"a": "abc"}"#, true),
        (r#"{"a": {"$regex": "^AB", "$options": "i"}}"#, r#"{"a": "abc"}"#, true),
        (r#"{"a": {"$options": "i", "$regex": "^AB"}}"#, r#"{"a": "abc"}"#, true),
        (
            r#"{"a": {"$regex": {"$regularExpression": {"pattern": "^AB", "options": "i"}}}}"#,
            r#"{"a": "abc"}"#,
            true,
        ),
        (
            r#"{"a": {"$regex": {"$regularExpression": {"pattern": "^AB", "options": ""}}, "$options": "i"}}"#,
            r#"{"a": "abc"}"#,
            true,
        ),
        (
            r#"{"a": {"$regularExpression": {"pattern": "b", "options": ""}}}"#,
            r#"{"a": ["x", "abc"]}"#,
            true,
        ),
        (
            r#"{"a": {"$regularExpression": {"pattern": "b", "options": ""}}}"#,
            r#"{"a": {"$symbol": "abc"}}"#,
            true,
        ),
        (r#"{"a": {"$regularExpression": {"pattern": "b", "options": ""}}}"#, r#"{"a": 1}"#, false),
        (
            r#"{"a": {"$regularExpression": {"pattern": "b", "options": "i"}}}"#,
            r#"{"a": {"$regularExpression": {"pattern": "b", "options": "i"}}}"#,
            true,
        ),
        (
            r#"{"a": {"$regularExpression": {"pattern": "b", "options": "i"}}}"#,
            r#"{"a": {"$regularExpression": {"pattern": "b", "options": ""}}}"#,
            false,
        ),
        (r#"{"$expr": true}"#, r#"{}"#, true),
        (r#"{"$expr": false}"#, r#"{}"#, false),
        (r#"{"$where": "true"}"#, r#"{}"#, true),
        (r#"{"$where": {"$code": "false"}}"#, r#"{}"#, false),
        (r#"{"$text": {"$search": "cat"}}"#, r#"{"t": "a cat"}"#, true),
        (
            r#"{"$text": {"$search": "dog", "$language": "en", "$caseSensitive": true, "$diacriticSensitive": false}}"#,
            r#"{"t": "a cat"}"#,
            false,
        ),
        (r#"{"$jsonSchema": {"required": ["a"]}}"#, r#"{"a": 1}"#, true),
        (r#"{"$jsonSchema": {"required": ["a"]}}"#, r#"{"b": 1}"#, false),
        (r#"{"$or": [{"$expr": false}, {"$jsonSchema": {}}]}"#, r#"{}"#, true),
    ]);
}

#[test]
fn array_operators() {
    check(&[
        (r#"{"a": {"$all": [1, 2]}}"#, r#"{"a": [2, 3, 1]}"#, true),
        (r#"{"a": {"$all": [1, 2]}}"#, r#"{"a": [2, 3]}"#, false),
        (r#"{"a": {"$all": []}}"#, r#"{"a": []}"#, false),
        (r#"{"a": {"$all": [1]}}"#, r#"{"a": 1}"#, true),
        (r#"{"a": {"$all": [[1, 2]]}}"#, r#"{"a": [1, 2]}"#, true),
        (r#"{"a": {"$all": [[1, 2]]}}"#, r#"{"a": [[1, 2], 3]}"#, true),
        (
            r#"{"a": {"$all": [{"$regularExpression": {"pattern": "^x", "options": ""}}]}}"#,
            r#"{"a": ["xa"]}"#,
            true,
        ),
        (
            r#"{"a": {"$all": [{"$elemMatch": {"b": 1}}, {"$elemMatch": {"b": 2}}]}}"#,
            r#"{"a": [{"b": 2}, {"b": 1}]}"#,
            true,
        ),
        (
            r#"{"a": {"$all": [{"$elemMatch": {"b": 1}}, {"$elemMatch": {"b": 3}}]}}"#,
            r#"{"a": [{"b": 2}, {"b": 1}]}"#,
            false,
        ),
        (r#"{"a": {"$size": 2}}"#, r#"{"a": [1, 2]}"#, true),
        (r#"{"a": {"$size": 2}}"#, r#"{"a": [[1, 2]]}"#, false),
        (r#"{"a": {"$size": 0}}"#, r#"{"a": []}"#, true),
        (r#"{"a": {"$size": 1}}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$size": 2.0}}"#, r#"{"a": [1, 2]}"#, true),
        (r#"{"a.b": {"$size": 1}}"#, r#"{"a": [{"b": [1]}, {"b": [1, 2]}]}"#, true),
        (
            r#"{"a": {"$elemMatch": {"x": 1, "y": {"$gt": 2}}}}"#,
            r#"{"a": [{"x": 1, "y": 1}, {"x": 2, "y": 3}]}"#,
            false,
        ),
        (
            r#"{"a": {"$elemMatch": {"x": 1, "y": {"$gt": 2}}}}"#,
            r#"{"a": [{"x": 1, "y": 3}]}"#,
            true,
        ),
        (
            r#"{"a.b": {"$elemMatch": {"x": 1, "y": {"$gt": 2}}}}"#,
            r#"{"a": {"b": [{"x": 1, "y": 3}]}}"#,
            true,
        ),
        (
            r#"{"a.b": {"$elemMatch": {"x": 1, "y": {"$gt": 2}}}}"#,
            r#"{"a": [{"b": [{"x": 1, "y": 3}]}]}"#,
            true,
        ),
        (
            r#"{"a.b": {"$elemMatch": {"x": 1, "y": {"$gt": 2}}}}"#,
            r#"{"a": [{"b": {"x": 1, "y": 3}}]}"#,
            false,
        ),
        (r#"{"a": {"$elemMatch": {"$gt": 1, "$lt": 3}}}"#, r#"{"a": [0, 5]}"#, false),
        (r#"{"a": {"$elemMatch": {"$gt": 1, "$lt": 3}}}"#, r#"{"a": [0, 2]}"#, true),
        (r#"{"a": {"$elemMatch": {"$eq": 1}}}"#, r#"{"a": [[1]]}"#, false),
        (r#"{"a": {"$elemMatch": {"$not": {"$gt": 5}}}}"#, r#"{"a": [7, 3]}"#, true),
        (r#"{"a": {"$elemMatch": {"$regex": "b", "$options": "i"}}}"#, r#"{"a": ["B"]}"#, true),
        (r#"{"a": {"$elemMatch": {"0": 1}}}"#, r#"{"a": [[1, 2]]}"#, true),
        (r#"{"a": {"$elemMatch": {"x": 1}}}"#, r#"{"a": [1, {"x": 1}]}"#, true),
        (r#"{"a": {"$elemMatch": {"x": 1}}}"#, r#"{"a": {"x": 1}}"#, false),
        (r#"{"a": {"$elemMatch": {"$and": [{"x": 1}]}}}"#, r#"{"a": [{"x": 1}]}"#, true),
        (
            r#"{"a": {"$elemMatch": {"x": {"$elemMatch": {"y": 1}}}}}"#,
            r#"{"a": [{"x": [{"y": 1}]}]}"#,
            true,
        ),
    ]);
}

#[test]
fn bit_operators() {
    check(&[
        (r#"{"a": {"$bitsAllSet": 5}}"#, r#"{"a": 7}"#, true),
        (r#"{"a": {"$bitsAllSet": 5}}"#, r#"{"a": 6}"#, false),
        (r#"{"a": {"$bitsAllSet": [0, 2]}}"#, r#"{"a": 5}"#, true),
        (r#"{"a": {"$bitsAllClear": [1]}}"#, r#"{"a": 5}"#, true),
        (r#"{"a": {"$bitsAnySet": [1, 2]}}"#, r#"{"a": 4}"#, true),
        (r#"{"a": {"$bitsAnyClear": [0, 2]}}"#, r#"{"a": 5}"#, false),
        (r#"{"a": {"$bitsAllSet": [70]}}"#, r#"{"a": -1}"#, true),
        (r#"{"a": {"$bitsAllClear": [70]}}"#, r#"{"a": 1}"#, true),
        (r#"{"a": {"$bitsAllSet": [0]}}"#, r#"{"a": 1.5}"#, false),
        (r#"{"a": {"$bitsAllSet": [0]}}"#, r#"{"a": 1.0}"#, true),
        (r#"{"a": {"$bitsAllSet": [0]}}"#, r#"{"a": {"$numberDouble": "1e30"}}"#, false),
        (r#"{"a": {"$bitsAllSet": [0]}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, false),
        (r#"{"a": {"$bitsAllSet": [0]}}"#, r#"{"a": "1"}"#, false),
        (
            r#"{"a": {"$bitsAllSet": [0, 9]}}"#,
            r#"{"a": {"$binary": {"base64": "AQI=", "subType": "00"}}}"#,
            true,
        ),
        (
            r#"{"a": {"$bitsAnySet": [20]}}"#,
            r#"{"a": {"$binary": {"base64": "AQI=", "subType": "00"}}}"#,
            false,
        ),
        (
            r#"{"a": {"$bitsAllSet": {"$binary": {"base64": "BQ==", "subType": "00"}}}}"#,
            r#"{"a": 5}"#,
            true,
        ),
        (r#"{"a": {"$bitsAllSet": []}}"#, r#"{"a": 0}"#, true),
        (r#"{"a": {"$bitsAnySet": []}}"#, r#"{"a": 1}"#, false),
        (r#"{"a": {"$bitsAnySet": 1}}"#, r#"{"a": [2, 3]}"#, true),
    ]);
}

#[test]
fn geospatial_operators_go_to_the_integration_point() {
    check(&[
        (r#"{"p": {"$geoWithin": {"$box": [[0, 0], [1, 1]]}}}"#, r#"{"p": {"x": 1}}"#, true),
        (r#"{"p": {"$geoWithin": {"$geometry": {"type": "Point"}}}}"#, r#"{"p": 5}"#, false),
        (
            r#"{"p": {"$near": [0, 0], "$maxDistance": 5, "$minDistance": 1}}"#,
            r#"{"p": {"x": 1}}"#,
            true,
        ),
        (r#"{"p": {"$nearSphere": {"$geometry": {}}}}"#, r#"{"q": 1}"#, false),
        (r#"{"p": {"$geoWithin": {"$polygon": []}}}"#, r#"{"p": [{"x": 1}]}"#, true),
        // `$within`, the old name of `$geoWithin` (009 T035, measured on a `refrouter` 9.0.4).
        (r#"{"p": {"$within": {"$box": [[0, 0], [1, 1]]}}}"#, r#"{"p": {"x": 1}}"#, true),
    ]);
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let f = bytes(r#"{"p": {"$geoIntersects": {"$geometry": {}}}}"#);
    let d = bytes(r#"{"p": 1}"#);
    let parsed = Filter::parse(raw(&f), FilterUse::Query).unwrap();
    assert!(parsed.matches(raw(&d), &ctx).is_err());
}

#[test]
fn sample_rate_uses_the_random_source() {
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    ext.random.set(0.4);
    assert!(matches_with(r#"{"$sampleRate": 0.5}"#, "{}", &ctx));
    ext.random.set(0.6);
    assert!(!matches_with(r#"{"$sampleRate": 0.5}"#, "{}", &ctx));
    assert!(!matches_with(r#"{"$sampleRate": 0}"#, "{}", &ctx));
}

#[test]
fn collation_orders_strings() {
    let ext = TestExt::default();
    let coll = Collation::for_locale("en").unwrap();
    let ctx = MatchContext { collation: Some(&coll), ext: &ext, vars: None };
    assert!(matches_with(r#"{"a": "b"}"#, r#"{"a": "b"}"#, &ctx));
    assert!(matches_with(r#"{"a": {"$lt": "B"}}"#, r#"{"a": "a"}"#, &ctx));
    assert!(matches_with(r#"{"a": {"$in": ["b"]}}"#, r#"{"a": ["a", "b"]}"#, &ctx));
    // Regular expressions ignore the collation.
    assert!(!matches_with(r#"{"a": {"$regex": "B"}}"#, r#"{"a": "b"}"#, &ctx));
}

#[test]
fn positional_match() {
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let pos = |filter: &str, doc: &str| {
        let f = bytes(filter);
        let d = bytes(doc);
        Filter::parse(raw(&f), FilterUse::Query).unwrap().match_position(raw(&d), &ctx).unwrap()
    };
    assert_eq!(pos(r#"{"a": 3}"#, r#"{"a": [1, 2, 3]}"#), Some(Some(2)));
    assert_eq!(pos(r#"{"a.b": 2}"#, r#"{"a": [{"b": 1}, {"b": 2}]}"#), Some(Some(1)));
    assert_eq!(pos(r#"{"a": {"$elemMatch": {"$gt": 1}}}"#, r#"{"a": [1, 5]}"#), Some(Some(1)));
    assert_eq!(
        pos(r#"{"a": {"$elemMatch": {"b": 1}}}"#, r#"{"a": [{"b": 0}, {"b": 1}]}"#),
        Some(Some(1))
    );
    assert_eq!(pos(r#"{"a": 1}"#, r#"{"a": 1}"#), Some(None));
    assert_eq!(pos(r#"{"a": 1}"#, r#"{"a": 2}"#), None);
    assert_eq!(
        pos(r#"{"$or": [{"a": 9, "b": 1}, {"a": 2}]}"#, r#"{"a": [1, 2, 9], "b": 0}"#),
        Some(Some(1))
    );
    assert_eq!(pos(r#"{"a": {"$ne": 5}}"#, r#"{"a": [1, 2]}"#), Some(None));
    assert_eq!(pos(r#"{"a": {"$all": [2]}}"#, r#"{"a": [1, 2]}"#), Some(Some(1)));
}

#[test]
fn without_extensions_the_engines_report_their_absence() {
    let ctx = MatchContext { collation: None, ext: &NoExtensions, vars: None };
    let run = |filter: &str| {
        let f = bytes(filter);
        let d = bytes(r#"{"p": [1, 2]}"#);
        Filter::parse(raw(&f), FilterUse::Query).unwrap().matches(raw(&d), &ctx)
    };
    assert_eq!(run(r#"{"$text": {"$search": "x"}}"#).unwrap_err().code, codes::INDEX_NOT_FOUND);
    assert_eq!(
        run(r#"{"p": {"$near": [0, 0]}}"#).unwrap_err().code,
        codes::NO_QUERY_EXECUTION_PLANS
    );
    assert_eq!(
        run(r#"{"p": {"$geoWithin": {"$box": []}}}"#).unwrap_err().code,
        codes::INTERNAL_ERROR
    );
    assert_eq!(run(r#"{"$expr": 1}"#).unwrap_err().code, codes::INTERNAL_ERROR);
    assert_eq!(run(r#"{"$where": "1"}"#).unwrap_err().code, codes::INTERNAL_ERROR);
    assert!(run(r#"{"$sampleRate": 0.5}"#).unwrap());
    assert!(run(r#"{"p": {"$regex": "(a+)+$"}}"#).is_ok());
    let f = bytes(r#"{"p": {"$regex": "^(a+)+$"}}"#);
    let d = bytes(&format!(r#"{{"p": "{}b"}}"#, "a".repeat(40)));
    let e = Filter::parse(raw(&f), FilterUse::Query).unwrap().matches(raw(&d), &ctx).unwrap_err();
    assert_eq!(e.code, codes::BAD_VALUE);
}

#[test]
fn malformed_filters_fail_like_refdb() {
    let q = FilterUse::Query;
    let bad = codes::BAD_VALUE;
    let cases: &[(&str, ErrorCode, &str)] = &[
        (
            r#"{"$foo": 1}"#,
            bad,
            "unknown top level operator: $foo. If you have a field name that starts with a '$' symbol, consider using $getField or $setField.",
        ),
        (r#"{"a": {"$foo": 1}}"#, bad, "unknown operator: $foo"),
        (r#"{"$and": 1}"#, bad, "$and argument must be an array"),
        (r#"{"$or": [1]}"#, bad, "$or argument's entries must be objects"),
        (r#"{"$nor": []}"#, bad, "$nor argument must be a non-empty array"),
        (r#"{"a": {"$in": 1}}"#, bad, "$in needs an array"),
        (r#"{"a": {"$nin": 1}}"#, bad, "$nin needs an array"),
        (r#"{"a": {"$in": [{"$gt": 1}]}}"#, bad, "cannot nest $ under $in"),
        (r#"{"a": {"$eq": {"$undefined": true}}}"#, bad, "cannot compare to undefined"),
        (
            r#"{"a": {"$ne": {"$regularExpression": {"pattern": "x", "options": ""}}}}"#,
            bad,
            "Can't have regex as arg to $ne.",
        ),
        (r#"{"a": {"$not": 1}}"#, bad, "$not argument must be a regex or an object"),
        (r#"{"a": {"$not": {}}}"#, bad, "$not argument must be a non-empty object"),
        (r#"{"a": {"$not": {"b": 1}}}"#, bad, "unknown operator: b"),
        (r#"{"a": {"$type": "nope"}}"#, bad, "Unknown type name alias: nope"),
        (r#"{"a": {"$type": 99}}"#, bad, "Invalid numerical type code: 99"),
        (
            r#"{"a": {"$type": true}}"#,
            codes::TYPE_MISMATCH,
            "type must be represented as a number or a string",
        ),
        (r#"{"a": {"$type": []}}"#, codes::FAILED_TO_PARSE, "a must match at least one type"),
        (
            r#"{"a": {"$size": "1"}}"#,
            bad,
            "Failed to parse $size. Expected a number in: $size: \"1\"",
        ),
        (
            r#"{"a": {"$size": -1}}"#,
            bad,
            "Failed to parse $size. Expected a non-negative number in: $size: -1",
        ),
        (r#"{"a": {"$size": 1.5}}"#, bad, "Failed to parse $size. Expected an integer: $size: 1.5"),
        (r#"{"a": {"$mod": 1}}"#, bad, "malformed mod, needs to be an array"),
        (r#"{"a": {"$mod": [4]}}"#, bad, "malformed mod, not enough elements"),
        (r#"{"a": {"$mod": [4, 1, 2]}}"#, bad, "malformed mod, too many elements"),
        (r#"{"a": {"$mod": ["4", 1]}}"#, bad, "malformed mod, divisor not a number"),
        (r#"{"a": {"$mod": [4, "1"]}}"#, bad, "malformed mod, remainder not a number"),
        (r#"{"a": {"$mod": [0, 1]}}"#, bad, "divisor cannot be 0"),
        (
            r#"{"a": {"$mod": [{"$numberDouble": "NaN"}, 1]}}"#,
            bad,
            "malformed mod, divisor value is invalid :: caused by :: Unable to coerce NaN/Inf to integral type",
        ),
        (r#"{"a": {"$regex": 1}}"#, bad, "$regex has to be a string"),
        (r#"{"a": {"$options": "i"}}"#, bad, "$options needs a $regex"),
        (r#"{"a": {"$regex": "x", "$options": 1}}"#, bad, "$options has to be a string"),
        (
            r#"{"a": {"$regex": {"$regularExpression": {"pattern": "x", "options": "i"}}, "$options": "m"}}"#,
            codes::REGEX_OPTIONS_AFTER,
            "options set in both $regex and $options",
        ),
        (r#"{"a": {"$all": 1}}"#, bad, "$all needs an array"),
        (
            r#"{"a": {"$all": [{"$elemMatch": {"b": 1}}, 2]}}"#,
            bad,
            "$all/$elemMatch has to be consistent",
        ),
        (r#"{"a": {"$all": [{"$gt": 1}]}}"#, bad, "no $ expressions in $all"),
        (r#"{"a": {"$elemMatch": 1}}"#, bad, "$elemMatch needs an Object"),
        (r#"{"a": {"$elemMatch": {"$gt": 1, "b": 1}}}"#, bad, "unknown operator: b"),
        (
            r#"{"a": {"$elemMatch": {"$where": "1"}}}"#,
            bad,
            "$where can only be applied to the top-level document",
        ),
        (
            r#"{"a": {"$elemMatch": {"$text": {"$search": "x"}}}}"#,
            bad,
            "$text can only be applied to the top-level document",
        ),
        (
            r#"{"a": {"$elemMatch": {"$expr": 1}}}"#,
            bad,
            "$expr can only be applied to the top-level document",
        ),
        (
            r#"{"a": {"$elemMatch": {"b": 1, "$expr": 1}}}"#,
            bad,
            "$expr can only be applied to the top-level document",
        ),
        (
            r#"{"a": {"$bitsAllSet": "x"}}"#,
            bad,
            "a takes an Array, a number, or a BinData but received: $bitsAllSet: \"x\"",
        ),
        (
            r#"{"a": {"$bitsAllSet": -1}}"#,
            codes::FAILED_TO_PARSE,
            "Expected a non-negative number in: $bitsAllSet: -1",
        ),
        (
            r#"{"a": {"$bitsAllSet": 1.5}}"#,
            codes::FAILED_TO_PARSE,
            "Expected an integer: $bitsAllSet: 1.5",
        ),
        (
            r#"{"a": {"$bitsAnySet": [-1]}}"#,
            bad,
            "Failed to parse bit position. Expected a non-negative number in: 0: -1",
        ),
        (
            r#"{"a": {"$bitsAnySet": ["x"]}}"#,
            bad,
            "Failed to parse bit position. Expected a number in: 0: \"x\"",
        ),
        (r#"{"$sampleRate": "x"}"#, bad, "argument to $sampleRate must be a numeric type"),
        (r#"{"$sampleRate": 2}"#, bad, "numeric argument to $sampleRate must be in [0, 1]"),
        (
            r#"{"$alwaysTrue": 2}"#,
            codes::FAILED_TO_PARSE,
            "$alwaysTrue must be an integer value of 1",
        ),
        (r#"{"$where": 1}"#, bad, "$where got bad type"),
        (r#"{"$text": 1}"#, bad, "$text expects an object"),
        (r#"{"$text": {}}"#, codes::NO_SUCH_KEY, "Missing expected field \"$search\""),
        (
            r#"{"$text": {"$search": 1}}"#,
            codes::TYPE_MISMATCH,
            "\"$search\" had the wrong type. Expected string, found int",
        ),
        (
            r#"{"$text": {"$search": "x", "$caseSensitive": 1}}"#,
            codes::TYPE_MISMATCH,
            "\"$caseSensitive\" had the wrong type. Expected bool, found int",
        ),
        (r#"{"$text": {"$search": "x", "$foo": 1}}"#, bad, "extra fields in $text"),
        (r#"{"$jsonSchema": 1}"#, codes::TYPE_MISMATCH, "$jsonSchema must be an object"),
        (r#"{"p": {"$geoWithin": 1}}"#, bad, "geometry must be an object"),
        (r#"{"p": {"$geoIntersects": {"$foo": []}}}"#, bad, "unknown geo specifier: $foo: []"),
        (r#"{"p": {"$within": 1}}"#, bad, "geometry must be an object"),
        (r#"{"p": {"$maxDistance": 1}}"#, bad, "unknown operator: $maxDistance"),
        (
            r#"{"p": {"$geoWithin": {"$box": []}, "$maxDistance": 1}}"#,
            bad,
            "can't parse extra field: $maxDistance: 1",
        ),
        (
            r#"{"p": {"$near": [0, 0], "$minDistance": -1}}"#,
            codes::NEAR_MIN_NEGATIVE,
            "$minDistance must be non-negative",
        ),
        (
            r#"{"a": {"$regex": "("}}"#,
            codes::REGEX_INVALID,
            "Regular expression is invalid: missing closing parenthesis",
        ),
    ];
    for (filter, code, message) in cases {
        let (c, m) = parse_error(filter, q);
        assert_eq!((c, m.as_str()), (*code, *message), "{filter}");
    }
}

#[test]
fn contexts_restrict_operators() {
    let v = FilterUse::Validator;
    assert_eq!(parse_error(r#"{"$where": "1"}"#, v).1, "$where is not allowed in this context");
    assert_eq!(
        parse_error(r#"{"$text": {"$search": "x"}}"#, v).1,
        "$text is not allowed in this context"
    );
    assert_eq!(
        parse_error(r#"{"p": {"$near": [0, 0]}}"#, v).1,
        "$near is not allowed in this context"
    );
    assert_eq!(
        parse_error(
            r#"{"$and": [{"a": {"$elemMatch": {"$and": [{"$where": "1"}]}}}]}"#,
            FilterUse::Query
        )
        .1,
        "$where can only be applied to the top-level document"
    );
    assert_eq!(
        parse_error(r#"{"$where": "1"}"#, FilterUse::Nested).1,
        "$where is not allowed in this context"
    );
    let f = bytes(r#"{"$expr": 1, "$jsonSchema": {}, "$or": [{"$where": "1"}]}"#);
    assert!(Filter::parse(raw(&f), FilterUse::Query).is_ok());
    assert!(
        format!("{:?}", Filter::parse(raw(&f), FilterUse::Query).unwrap().expr()).contains("Where")
    );
}

/// An array reached with a position at the end of the path is the value, not its elements; one
/// reached through a field is expanded (measured on a `refrouter` 9.0.4).
#[test]
fn an_array_reached_by_position_is_not_expanded() {
    check(&[
        (r#"{"a.0": 1}"#, r#"{"a": [[1, 2]]}"#, false),
        (r#"{"a.0": {"$gt": 0}}"#, r#"{"a": [[3], [1, 2]]}"#, false),
        (r#"{"a.1": 1}"#, r#"{"a": [1, [1]]}"#, false),
        (r#"{"a.0": {"$ne": 1}}"#, r#"{"a": [[1, 2]]}"#, true),
        (r#"{"a.0": {"$size": 2}}"#, r#"{"a": [[1, 2]]}"#, true),
        (r#"{"a.0": {"$elemMatch": {"$eq": 1}}}"#, r#"{"a": [[1, 2]]}"#, true),
        (r#"{"a.0.0": 1}"#, r#"{"a": [[[1]]]}"#, false),
        (r#"{"a.0.0": 1}"#, r#"{"a": [[1, 2]]}"#, true),
        (r#"{"a.0.b": 1}"#, r#"{"a": [[{"b": 1}]]}"#, true),
        (r#"{"a.0": 1}"#, r#"{"a": {"0": [1, 2]}}"#, true),
        (r#"{"a.0": 1}"#, r#"{"a": [{"0": [1, 2]}]}"#, true),
        (r#"{"a.1": [1]}"#, r#"{"a": [1, [1]]}"#, true),
    ]);
}

/// `MinKey` and `MaxKey` compare with every value, a missing one (as `null`) and NaN too.
#[test]
fn min_and_max_key_bound_everything() {
    check(&[
        (r#"{"a": {"$gt": {"$minKey": 1}}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$gt": {"$minKey": 1}}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, true),
        (r#"{"a": {"$gt": {"$minKey": 1}}}"#, r#"{"a": {"$minKey": 1}}"#, false),
        (r#"{"a": {"$gte": {"$minKey": 1}}}"#, r#"{"a": {"$minKey": 1}}"#, true),
        (r#"{"a": {"$lt": {"$maxKey": 1}}}"#, r#"{"b": 1}"#, true),
        (r#"{"a": {"$lt": {"$maxKey": 1}}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, true),
        (r#"{"a": {"$lt": {"$maxKey": 1}}}"#, r#"{"a": {"$maxKey": 1}}"#, false),
        (r#"{"a": {"$lt": {"$minKey": 1}}}"#, r#"{"b": 1}"#, false),
        (r#"{"a": {"$gt": {"$maxKey": 1}}}"#, r#"{"a": {"$numberDouble": "NaN"}}"#, false),
    ]);
}

/// `$options` beside a `$regex` that is a regular expression, in both orders, and the operator
/// documents of the geospatial operators, as a `refrouter` 9.0.4 answers them (measured with the
/// Node driver, which sends these documents as they are written).
#[test]
fn regex_options_and_geo_operator_documents() {
    let q = FilterUse::Query;
    let re = |o: &str| format!(r#"{{"$regularExpression": {{"pattern": "x", "options": "{o}"}}}}"#);
    let after = format!(r#"{{"a": {{"$regex": {}, "$options": ""}}}}"#, re("i"));
    assert_eq!(parse_error(&after, q).0, codes::REGEX_OPTIONS_AFTER);
    let before = format!(r#"{{"a": {{"$options": "m", "$regex": {}}}}}"#, re("i"));
    assert_eq!(parse_error(&before, q).0, codes::REGEX_OPTIONS_BEFORE);
    check(&[
        (&format!(r#"{{"a": {{"$options": "", "$regex": {}}}}}"#, re("i")), r#"{"a": "X"}"#, true),
        (&format!(r#"{{"a": {{"$regex": {}, "$options": "i"}}}}"#, re("")), r#"{"a": "X"}"#, true),
    ]);
    let bad = codes::BAD_VALUE;
    let gj = r#"{"$geometry": {"type": "Point", "coordinates": [0, 0]}"#;
    let cases: Vec<(String, ErrorCode, String)> = vec![
        (r#"{"a": {"$geoWithin": {}}}"#.into(), bad, "geo query doesn't have any geometry".into()),
        (r#"{"a": {"$geoWithin": {"$geometry": 1}}}"#.into(), bad, "unknown geo specifier: $geometry: 1".into()),
        (r#"{"a": {"$geoWithin": {"$box": 1}}}"#.into(), bad, "unknown geo specifier: $box: 1".into()),
        (r#"{"a": {"$gt": 1, "$geoWithin": {"$box": []}}}"#.into(), bad, "can't parse extra field: $geoWithin: { $box: [] }".into()),
        (r#"{"a": {"$geoWithin": {"$box": [], "$x": 1}}}"#.into(), bad, "$geoWithin doesn't accept multiple shapes $geoWithin: { $box: [], $x: 1 }".into()),
        (format!(r#"{{"a": {{"$geoIntersects": {}, "x": 1}}}}}}"#, &gj[..gj.len()]), bad, "unknown geo specifier: x: 1".into()),
        (r#"{"a": {"$near": [0, 0], "$minDistance": "x"}}"#.into(), codes::NEAR_MIN_NOT_NUMBER, "$minDistance must be a number".into()),
        (r#"{"a": {"$near": [0, 0], "$maxDistance": "x"}}"#.into(), codes::NEAR_MAX_NOT_NUMBER, "$maxDistance must be a number".into()),
        (r#"{"a": {"$near": [0, 0], "$maxDistance": -1}}"#.into(), codes::NEAR_MAX_NEGATIVE, "$maxDistance must be non-negative".into()),
        (r#"{"a": {"$near": [0, 0], "$gt": 1}}"#.into(), codes::NEAR_INVALID_ARGUMENT, "invalid argument in geo near query: $gt".into()),
        (format!(r#"{{"a": {{"$near": {}, "$maxDistance": -1}}}}}}"#, gj), bad, "$maxDistance must be non-negative".into()),
        (format!(r#"{{"a": {{"$near": {}, "$minDistance": "x"}}}}}}"#, gj), bad, "$minDistance must be a number".into()),
        (format!(r#"{{"a": {{"$near": {}, "$foo": 1}}}}}}"#, gj), bad, "invalid argument in geo near query: $foo".into()),
        (format!(r#"{{"a": {{"$near": {}}}, "$maxDistance": 1}}}}"#, gj), bad, "geo near accepts just one argument when querying for a GeoJSON point. Extra field found: $maxDistance: 1".into()),
        (r#"{"a": {"$near": {"$geometry": 1}}}"#.into(), bad, "$geometry is required for geo near query".into()),
        (r#"{"a": {"$gt": 1, "$near": [0, 0]}}"#.into(), bad, "near must be first in: { $gt: 1, $near: [ 0, 0 ] }".into()),
        (r#"{"a": {"$near": "x"}}"#.into(), bad, "near must be first in: { $near: \"x\" }".into()),
        (r#"{"a": {"$minDistance": 1, "$near": [0, 0]}}"#.into(), bad, "unknown operator: $minDistance".into()),
        (r#"{"a": {"$near": []}}"#.into(), bad, "invalid point provided to geo near query: {}".into()),
        (r#"{"a": {"$near": ["x", 0]}}"#.into(), bad, "invalid point provided to geo near query: { 0: \"x\", 1: 0 }".into()),
        (r#"{"a": {"$near": {"x": 1}}}"#.into(), bad, "invalid point provided to geo near query: { x: 1 }".into()),
        (r#"{"a": {"$near": [0, 0]}, "b": {"$nearSphere": [0, 0]}}"#.into(), bad, "Too many geoNear expressions".into()),
        (r#"{"$or": [{"a": {"$near": [0, 0]}}, {"b": 1}]}"#.into(), bad, "geo $near must be top-level expr".into()),
        (r#"{"$nor": [{"a": {"$near": [0, 0]}}]}"#.into(), bad, "geo $near must be top-level expr".into()),
        (r#"{"a": {"$not": {"$near": [0, 0]}}}"#.into(), bad, "geo $near must be top-level expr".into()),
        (r#"{"a": {"$elemMatch": {"$near": [0, 0]}}}"#.into(), bad, "geo $near must be top-level expr".into()),
        (r#"{"a": {"$elemMatch": {"b": {"$near": [0, 0]}}}}"#.into(), bad, "geo $near must be top-level expr".into()),
        (r#"{"a": {"$where": "1"}}"#.into(), bad, "$where cannot be applied to a field".into()),
        (r#"{"a": {"$comment": "x"}}"#.into(), bad, "unknown operator: $comment".into()),
        (r#"{"a": {"$regex": "a", "$options": "z"}}"#.into(), codes::REGEX_INVALID_OPTION, " invalid flag in regex options: z".into()),
        (r#"{"a": {"$regularExpression": {"pattern": "a", "options": "z"}}}"#.into(), codes::REGEX_INVALID_OPTION, "invalid flag in regex options: z".into()),
        (r#"{"a": {"$in": [{"$undefined": true}]}}"#.into(), bad, "Cannot insert undefined into InListData".into()),
        (r#"{"$not": {"a": 1}}"#.into(), bad, "unknown top level operator: $not. If you are trying to negate an entire expression, use $nor.".into()),
        (r#"{"a": {"$gt": {"$regularExpression": {"pattern": "x", "options": ""}}}}"#.into(), bad, "Can't have RegEx as arg to non-equality predicate over field 'a'.".into()),
        (r#"{"a": {"$type": "missing"}}"#.into(), bad, "'missing' is not a legal type name. To query for non-existence of a field, use {$exists:false}.".into()),
        (r#"{"a": {"$bitsAnySet": [1.5]}}"#.into(), bad, "Failed to parse bit position. Expected an integer: 0: 1.5".into()),
        (r#"{"$text": {"$search": "x", "$language": 1}}"#.into(), codes::TYPE_MISMATCH, "\"$language\" had the wrong type. Expected string, found int".into()),
        (r#"{"a": {"$mod": [{"$numberDouble": "1e300"}, 1]}}"#.into(), bad, "malformed mod, divisor value is invalid :: caused by :: Out of bounds coercing to integral value".into()),
    ];
    for (filter, code, message) in &cases {
        assert_eq!(parse_error(filter, q), (*code, message.clone()), "{filter}");
    }
    // `$near` at the top level, inside `$and`, or in a `$or` of one clause, and its distances.
    for filter in [
        r#"{"a": {"$near": [0, 0], "$maxDistance": 1, "$minDistance": 0}}"#,
        r#"{"$and": [{"a": {"$near": [0, 0]}}, {"b": 1}]}"#,
        r#"{"$or": [{"a": {"$near": [0, 0]}}]}"#,
        r#"{"a": {"$near": {"x": 1, "y": 2}}}"#,
        r#"{"a": {"$nearSphere": {"$geometry": {"type": "Point", "coordinates": [0, 0]}, "$maxDistance": 1, "$minDistance": 0}}}"#,
        r#"{"a": {"$geoWithin": {"$centerSphere": [[0, 0], 1]}}}"#,
    ] {
        let f = common::bytes(filter);
        assert!(Filter::parse(common::raw(&f), q).is_ok(), "{filter}");
    }
}

/// `matches` and `match_position` agree, also on `$type: "number"` over `MinKey`, which takes
/// none of the numeric types (the battery's `query-type`, 04-11-2026).
#[test]
fn matches_and_match_position_agree_on_type() {
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    for doc in [r#"{"a": {"$minKey": 1}}"#, r#"{"a": {"$maxKey": 1}}"#, r#"{"a": 1}"#] {
        let f = bytes(r#"{"a": {"$type": "number"}}"#);
        let filter = Filter::parse(raw(&f), FilterUse::Query).unwrap();
        let b = bytes(doc);
        let doc_raw = raw(&b);
        let whole = filter.matches(doc_raw, &ctx).unwrap();
        let positioned = filter.match_position(doc_raw, &ctx).unwrap().is_some();
        assert_eq!(whole, positioned, "{doc}");
        assert_eq!(whole, doc == r#"{"a": 1}"#, "{doc}");
    }
}
