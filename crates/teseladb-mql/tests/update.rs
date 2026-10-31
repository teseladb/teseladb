//! Update operators (006 T020, FR-012, FR-013): field and array operators, positional paths with
//! `arrayFilters`, the `$push` modifiers, replacement documents, upserts, the order RefDb
//! applies them in and the errors of conflicting paths and the immutable `_id`.

pub mod common;

use common::{TestExt, bytes, canon, json, raw};
use teseladb_bson::{RawValue, Value};
use teseladb_collation::Collation;
use teseladb_mql::update::{Now, Update, UpdateContext, upsert};
use teseladb_mql::{ErrorCode, Filter, FilterUse, MatchContext, MqlError, NoExtensions, codes};

const NOW: Now = Now { millis: 1_700_000_000_123, timestamp: (1_700_000_000, 7) };

/// The update `{"u": ...}` and its array filters `{"f": [...]}` read from one JSON document.
fn parse(update: &str, filters: Option<&str>) -> Result<Update, MqlError> {
    let u = bytes(&format!(r#"{{"u": {update}}}"#));
    let f = filters.map(|f| bytes(&format!(r#"{{"f": {f}}}"#)));
    let filters = f.as_ref().map(|b| match raw(b).get("f") {
        Some(RawValue::Array(a)) => a,
        _ => panic!("filters must be an array"),
    });
    Update::parse(raw(&u).get("u").unwrap(), filters)
}

fn run(
    update: &str,
    filters: Option<&str>,
    doc: &str,
    position: Option<usize>,
) -> Result<(String, bool), MqlError> {
    let ext = TestExt::default();
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position,
        now: NOW,
        insert: false,
    };
    let d = bytes(doc);
    let out = parse(update, filters)?.apply(raw(&d), &ctx)?;
    Ok((json(&out.doc), out.modified))
}

/// `(update, document, result)`.
fn check(cases: &[(&str, &str, &str)]) {
    for &(update, doc, want) in cases {
        let (got, modified) =
            run(update, None, doc, None).unwrap_or_else(|e| panic!("{update} on {doc}: {e}"));
        assert_eq!(got, canon(want), "{update} on {doc}");
        assert_eq!(modified, canon(doc) != canon(want), "modified: {update} on {doc}");
    }
}

fn error(update: &str, filters: Option<&str>, doc: &str) -> (ErrorCode, String) {
    let e = run(update, filters, doc, None).expect_err(update);
    (e.code, e.message)
}

#[test]
fn set_and_unset() {
    check(&[
        (r#"{"$set": {"a": 2}}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 2}"#),
        (r#"{"$set": {"a": 1}}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1}"#),
        (r#"{"$set": {"a": 1.0}}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1.0}"#),
        (r#"{"$set": {"b": 2}}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "a": 1, "b": 2}"#),
        (r#"{"$set": {"b.c.d": 2}}"#, r#"{"_id": 1}"#, r#"{"_id": 1, "b": {"c": {"d": 2}}}"#),
        (
            r#"{"$set": {"a.b": 2}}"#,
            r#"{"_id": 1, "a": {"c": 1}}"#,
            r#"{"_id": 1, "a": {"c": 1, "b": 2}}"#,
        ),
        (r#"{"$set": {"a.1": 9}}"#, r#"{"_id": 1, "a": [1, 2]}"#, r#"{"_id": 1, "a": [1, 9]}"#),
        (
            r#"{"$set": {"a.4": 9}}"#,
            r#"{"_id": 1, "a": [1]}"#,
            r#"{"_id": 1, "a": [1, null, null, null, 9]}"#,
        ),
        (
            r#"{"$set": {"a.0.b": 9}}"#,
            r#"{"_id": 1, "a": [{"b": 1}]}"#,
            r#"{"_id": 1, "a": [{"b": 9}]}"#,
        ),
        (
            r#"{"$set": {"a.1.b": 9}}"#,
            r#"{"_id": 1, "a": [{}]}"#,
            r#"{"_id": 1, "a": [{}, {"b": 9}]}"#,
        ),
        (
            r#"{"$set": {"z": 1, "b": 1, "a.10": 1, "a.2": 1}}"#,
            r#"{"_id": 1, "a": {}}"#,
            r#"{"_id": 1, "a": {"2": 1, "10": 1}, "b": 1, "z": 1}"#,
        ),
        (r#"{"$set": {"a": {"x": [1]}}}"#, r#"{"_id": 1}"#, r#"{"_id": 1, "a": {"x": [1]}}"#),
        (r#"{"$set": {"_id": 1}}"#, r#"{"_id": 1}"#, r#"{"_id": 1}"#),
        (r#"{"$set": {}}"#, r#"{"_id": 1}"#, r#"{"_id": 1}"#),
        (r#"{"$unset": {"a": ""}}"#, r#"{"_id": 1, "a": 1, "b": 2}"#, r#"{"_id": 1, "b": 2}"#),
        (r#"{"$unset": {"z": 1}}"#, r#"{"_id": 1}"#, r#"{"_id": 1}"#),
        (
            r#"{"$unset": {"a.b": 1}}"#,
            r#"{"_id": 1, "a": {"b": 1, "c": 2}}"#,
            r#"{"_id": 1, "a": {"c": 2}}"#,
        ),
        (
            r#"{"$unset": {"a.1": 1}}"#,
            r#"{"_id": 1, "a": [1, 2, 3]}"#,
            r#"{"_id": 1, "a": [1, null, 3]}"#,
        ),
        (
            r#"{"$unset": {"a.9": 1, "a.x": 1, "b.c": 1}}"#,
            r#"{"_id": 1, "a": [1], "b": 5}"#,
            r#"{"_id": 1, "a": [1], "b": 5}"#,
        ),
        (r#"{"$setOnInsert": {"a": 1}}"#, r#"{"_id": 1}"#, r#"{"_id": 1}"#),
    ]);
}

#[test]
fn arithmetic() {
    check(&[
        (r#"{"$inc": {"a": 1}}"#, r#"{"a": 1}"#, r#"{"a": 2}"#),
        (r#"{"$inc": {"a": -1.5}}"#, r#"{"a": 1}"#, r#"{"a": -0.5}"#),
        (r#"{"$inc": {"a": 1}}"#, r#"{}"#, r#"{"a": 1}"#),
        (
            r#"{"$inc": {"a.b": {"$numberLong": "2"}}}"#,
            r#"{}"#,
            r#"{"a": {"b": {"$numberLong": "2"}}}"#,
        ),
        (
            r#"{"$inc": {"a": 1}}"#,
            r#"{"a": 2147483647}"#,
            r#"{"a": {"$numberLong": "2147483648"}}"#,
        ),
        (
            r#"{"$inc": {"a": {"$numberLong": "1"}}}"#,
            r#"{"a": 1}"#,
            r#"{"a": {"$numberLong": "2"}}"#,
        ),
        (
            r#"{"$inc": {"a": 1}}"#,
            r#"{"a": {"$numberDecimal": "1.5"}}"#,
            r#"{"a": {"$numberDecimal": "2.5"}}"#,
        ),
        (
            r#"{"$inc": {"a": {"$numberDecimal": "0.1"}}}"#,
            r#"{"a": 0.5}"#,
            r#"{"a": {"$numberDecimal": "0.600000000000000"}}"#,
        ),
        (r#"{"$inc": {"a": 0}}"#, r#"{"a": 5}"#, r#"{"a": 5}"#),
        (r#"{"$mul": {"a": 3}}"#, r#"{"a": 2}"#, r#"{"a": 6}"#),
        (r#"{"$mul": {"a": 2.5}}"#, r#"{"a": 2}"#, r#"{"a": 5.0}"#),
        (r#"{"$mul": {"a": 3}}"#, r#"{}"#, r#"{"a": 0}"#),
        (r#"{"$mul": {"a": 3.0}}"#, r#"{}"#, r#"{"a": 0.0}"#),
        (r#"{"$mul": {"a": {"$numberLong": "3"}}}"#, r#"{}"#, r#"{"a": {"$numberLong": "0"}}"#),
        (
            r#"{"$mul": {"a": {"$numberDecimal": "3"}}}"#,
            r#"{}"#,
            r#"{"a": {"$numberDecimal": "0"}}"#,
        ),
        (
            r#"{"$mul": {"a": 2}}"#,
            r#"{"a": 2000000000}"#,
            r#"{"a": {"$numberLong": "4000000000"}}"#,
        ),
        (
            r#"{"$mul": {"a": {"$numberDecimal": "1.5"}}}"#,
            r#"{"a": {"$numberDecimal": "2.0"}}"#,
            r#"{"a": {"$numberDecimal": "3.00"}}"#,
        ),
        (r#"{"$min": {"a": 1}}"#, r#"{"a": 2}"#, r#"{"a": 1}"#),
        (r#"{"$min": {"a": 3}}"#, r#"{"a": 2}"#, r#"{"a": 2}"#),
        (r#"{"$min": {"a": 3}}"#, r#"{}"#, r#"{"a": 3}"#),
        (r#"{"$min": {"a": null}}"#, r#"{"a": 2}"#, r#"{"a": null}"#),
        (r#"{"$max": {"a": "x"}}"#, r#"{"a": 2}"#, r#"{"a": "x"}"#),
        (r#"{"$max": {"a": 1}}"#, r#"{"a": 1.0}"#, r#"{"a": 1.0}"#),
        (r#"{"$bit": {"a": {"and": 6}}}"#, r#"{"a": 5}"#, r#"{"a": 4}"#),
        (r#"{"$bit": {"a": {"or": 2, "xor": 1}}}"#, r#"{"a": 5}"#, r#"{"a": 6}"#),
        (
            r#"{"$bit": {"a": {"or": {"$numberLong": "8"}}}}"#,
            r#"{"a": 1}"#,
            r#"{"a": {"$numberLong": "9"}}"#,
        ),
        (r#"{"$bit": {"a": {"or": 3}}}"#, r#"{}"#, r#"{"a": 3}"#),
        (
            r#"{"$bit": {"a": {"xor": 1}}}"#,
            r#"{"a": {"$numberLong": "1"}}"#,
            r#"{"a": {"$numberLong": "0"}}"#,
        ),
    ]);
}

#[test]
fn rename_and_current_date() {
    check(&[
        (r#"{"$rename": {"a": "b"}}"#, r#"{"a": 1, "c": 2}"#, r#"{"c": 2, "b": 1}"#),
        (r#"{"$rename": {"a": "c"}}"#, r#"{"a": 1, "c": 2}"#, r#"{"c": 1}"#),
        (r#"{"$rename": {"z": "b"}}"#, r#"{"a": 1}"#, r#"{"a": 1}"#),
        (r#"{"$rename": {"a.b": "c.d"}}"#, r#"{"a": {"b": 1}}"#, r#"{"a": {}, "c": {"d": 1}}"#),
        (
            r#"{"$currentDate": {"d": true}}"#,
            r#"{}"#,
            r#"{"d": {"$date": {"$numberLong": "1700000000123"}}}"#,
        ),
        (
            r#"{"$currentDate": {"d": {"$type": "date"}}}"#,
            r#"{}"#,
            r#"{"d": {"$date": {"$numberLong": "1700000000123"}}}"#,
        ),
        (
            r#"{"$currentDate": {"t": {"$type": "timestamp"}}}"#,
            r#"{}"#,
            r#"{"t": {"$timestamp": {"t": 1700000000, "i": 7}}}"#,
        ),
    ]);
}

#[test]
fn array_operators() {
    check(&[
        (r#"{"$push": {"a": 3}}"#, r#"{"a": [1]}"#, r#"{"a": [1, 3]}"#),
        (r#"{"$push": {"a": 3}}"#, r#"{}"#, r#"{"a": [3]}"#),
        (r#"{"$push": {"a": [3]}}"#, r#"{"a": []}"#, r#"{"a": [[3]]}"#),
        (r#"{"$push": {"a": {"x": 1}}}"#, r#"{"a": []}"#, r#"{"a": [{"x": 1}]}"#),
        (r#"{"$push": {"a": {"$each": [2, 3]}}}"#, r#"{"a": [1]}"#, r#"{"a": [1, 2, 3]}"#),
        (r#"{"$push": {"a": {"$each": []}}}"#, r#"{}"#, r#"{"a": []}"#),
        (
            r#"{"$push": {"s": {"$each": [5, 1], "$sort": 1, "$slice": -3}}}"#,
            r#"{"s": [4, 9, 2]}"#,
            r#"{"s": [4, 5, 9]}"#,
        ),
        (
            r#"{"$push": {"s": {"$each": [5], "$sort": -1}}}"#,
            r#"{"s": [4, 9]}"#,
            r#"{"s": [9, 5, 4]}"#,
        ),
        (r#"{"$push": {"s": {"$each": [], "$slice": 1}}}"#, r#"{"s": [4, 9]}"#, r#"{"s": [4]}"#),
        (r#"{"$push": {"s": {"$each": [], "$slice": 0}}}"#, r#"{"s": [4, 9]}"#, r#"{"s": []}"#),
        (
            r#"{"$push": {"s": {"$each": [7, 8], "$position": 1}}}"#,
            r#"{"s": [1, 2]}"#,
            r#"{"s": [1, 7, 8, 2]}"#,
        ),
        (
            r#"{"$push": {"s": {"$each": [90, 80], "$position": -2}}}"#,
            r#"{"s": [50, 60, 20, 30, 70, 100]}"#,
            r#"{"s": [50, 60, 20, 30, 90, 80, 70, 100]}"#,
        ),
        (
            r#"{"$push": {"s": {"$each": [0], "$position": -9}}}"#,
            r#"{"s": [1]}"#,
            r#"{"s": [0, 1]}"#,
        ),
        (
            r#"{"$push": {"s": {"$each": [0], "$position": 9}}}"#,
            r#"{"s": [1]}"#,
            r#"{"s": [1, 0]}"#,
        ),
        (
            r#"{"$push": {"q": {"$each": [{"w": 5, "s": 8}, {"w": 6, "s": 7}, {"w": 7, "s": 6}], "$sort": {"s": -1}, "$slice": 3}}}"#,
            r#"{"q": [{"w": 1, "s": 10}, {"w": 2, "s": 8}, {"w": 3, "s": 5}, {"w": 4, "s": 6}]}"#,
            r#"{"q": [{"w": 1, "s": 10}, {"w": 2, "s": 8}, {"w": 5, "s": 8}]}"#,
        ),
        (
            r#"{"$push": {"q": {"$each": [{"x": {"y": 1}}], "$sort": {"x.y": 1}}}}"#,
            r#"{"q": [{"x": {"y": 2}}, 3]}"#,
            r#"{"q": [3, {"x": {"y": 1}}, {"x": {"y": 2}}]}"#,
        ),
        (r#"{"$addToSet": {"a": 2}}"#, r#"{"a": [1, 2]}"#, r#"{"a": [1, 2]}"#),
        (r#"{"$addToSet": {"a": 3}}"#, r#"{"a": [1, 2]}"#, r#"{"a": [1, 2, 3]}"#),
        (r#"{"$addToSet": {"a": 2.0}}"#, r#"{"a": [2]}"#, r#"{"a": [2]}"#),
        (
            r#"{"$addToSet": {"a": {"$each": [3, 3, 1, 4]}}}"#,
            r#"{"a": [1]}"#,
            r#"{"a": [1, 3, 4]}"#,
        ),
        (r#"{"$addToSet": {"a": [1]}}"#, r#"{}"#, r#"{"a": [[1]]}"#),
        (r#"{"$pop": {"a": 1}}"#, r#"{"a": [1, 2, 3]}"#, r#"{"a": [1, 2]}"#),
        (r#"{"$pop": {"a": -1}}"#, r#"{"a": [1, 2, 3]}"#, r#"{"a": [2, 3]}"#),
        (r#"{"$pop": {"a": 1}}"#, r#"{"a": []}"#, r#"{"a": []}"#),
        (r#"{"$pop": {"a": 1}}"#, r#"{}"#, r#"{}"#),
        (r#"{"$pull": {"a": 2}}"#, r#"{"a": [1, 2, 3, 2]}"#, r#"{"a": [1, 3]}"#),
        (r#"{"$pull": {"a": {"$gte": 2}}}"#, r#"{"a": [1, 2, 3]}"#, r#"{"a": [1]}"#),
        (r#"{"$pull": {"a": {"$in": ["x", 1]}}}"#, r#"{"a": [1, "x", "y"]}"#, r#"{"a": ["y"]}"#),
        (
            r#"{"$pull": {"a": {"b": 1}}}"#,
            r#"{"a": [{"b": 1, "c": 2}, {"b": 2}, 1]}"#,
            r#"{"a": [{"b": 2}, 1]}"#,
        ),
        (
            r#"{"$pull": {"a": {"b": {"$gt": 1}, "c": 1}}}"#,
            r#"{"a": [{"b": 2, "c": 1}, {"b": 2}]}"#,
            r#"{"a": [{"b": 2}]}"#,
        ),
        (r#"{"$pull": {"a": [1, 2]}}"#, r#"{"a": [[1, 2], [2, 1], 1]}"#, r#"{"a": [[2, 1], 1]}"#),
        (
            r#"{"$pull": {"a": {"$elemMatch": {"$gt": 1}}}}"#,
            r#"{"a": [[1, 2], [0]]}"#,
            r#"{"a": [[0]]}"#,
        ),
        (r#"{"$pull": {"a": 1}}"#, r#"{}"#, r#"{}"#),
        (r#"{"$pullAll": {"a": [1, 2]}}"#, r#"{"a": [1, 2, 3, 1.0]}"#, r#"{"a": [3]}"#),
        (r#"{"$pullAll": {"a": [1]}}"#, r#"{}"#, r#"{}"#),
    ]);
}

#[test]
fn positional_operators() {
    let ok = |update: &str, filters: Option<&str>, doc: &str, pos: Option<usize>, want: &str| {
        let (got, _) = run(update, filters, doc, pos).unwrap_or_else(|e| panic!("{update}: {e}"));
        assert_eq!(got, canon(want), "{update} on {doc}");
    };
    ok(r#"{"$set": {"a.$": 9}}"#, None, r#"{"a": [1, 2, 3]}"#, Some(1), r#"{"a": [1, 9, 3]}"#);
    ok(
        r#"{"$inc": {"a.$.n": 1}}"#,
        None,
        r#"{"a": [{"n": 1}, {"n": 5}]}"#,
        Some(1),
        r#"{"a": [{"n": 1}, {"n": 6}]}"#,
    );
    ok(r#"{"$set": {"a.$[]": 0}}"#, None, r#"{"a": [1, 2]}"#, None, r#"{"a": [0, 0]}"#);
    ok(
        r#"{"$inc": {"a.$[].n": 1}}"#,
        None,
        r#"{"a": [{"n": 1}, {}]}"#,
        None,
        r#"{"a": [{"n": 2}, {"n": 1}]}"#,
    );
    ok(
        r#"{"$set": {"a.$[].b.$[]": 0}}"#,
        None,
        r#"{"a": [{"b": [1, 2]}, {"b": [3]}]}"#,
        None,
        r#"{"a": [{"b": [0, 0]}, {"b": [0]}]}"#,
    );
    ok(
        r#"{"$set": {"a.$[x]": 0}}"#,
        Some(r#"[{"x": {"$gt": 1}}]"#),
        r#"{"a": [1, 2, 3]}"#,
        None,
        r#"{"a": [1, 0, 0]}"#,
    );
    ok(
        r#"{"$set": {"a.$[e].v": "y"}}"#,
        Some(r#"[{"e.k": 2}]"#),
        r#"{"a": [{"k": 1}, {"k": 2}]}"#,
        None,
        r#"{"a": [{"k": 1}, {"k": 2, "v": "y"}]}"#,
    );
    ok(
        r#"{"$set": {"a.$[e].b.$[f]": 0}}"#,
        Some(r#"[{"e.k": 1}, {"f": {"$lt": 5}}]"#),
        r#"{"a": [{"k": 1, "b": [1, 9]}, {"k": 2, "b": [1]}]}"#,
        None,
        r#"{"a": [{"k": 1, "b": [0, 9]}, {"k": 2, "b": [1]}]}"#,
    );
    ok(
        r#"{"$set": {"a.$[x]": 0}}"#,
        Some(r#"[{"x": {"$gt": 9}}]"#),
        r#"{"a": [1, 2]}"#,
        None,
        r#"{"a": [1, 2]}"#,
    );
    ok(r#"{"$unset": {"a.$[]": 1}}"#, None, r#"{"a": [1, 2]}"#, None, r#"{"a": [null, null]}"#);
    ok(
        r#"{"$push": {"a.$[].l": 1}}"#,
        None,
        r#"{"a": [{"l": []}, {}]}"#,
        None,
        r#"{"a": [{"l": [1]}, {"l": [1]}]}"#,
    );
    ok(
        r#"{"$set": {"a.$[x]": 0}}"#,
        Some(r#"[{"x": {"$in": [{"$regularExpression": {"pattern": "^b", "options": ""}}]}}]"#),
        r#"{"a": ["ab", "bc"]}"#,
        None,
        r#"{"a": ["ab", 0]}"#,
    );
}

#[test]
fn replacement_documents() {
    check(&[
        (r#"{"b": 2}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "b": 2}"#),
        (r#"{"b": 2, "_id": 1}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1, "b": 2}"#),
        (r#"{}"#, r#"{"_id": 1, "a": 1}"#, r#"{"_id": 1}"#),
        (r#"{"a": 1}"#, r#"{"a": 1}"#, r#"{"a": 1}"#),
    ]);
}

#[test]
fn collation_applies_to_set_operators() {
    let ext = TestExt::default();
    let coll = Collation::for_locale("en").unwrap();
    let ctx = UpdateContext {
        matcher: MatchContext { collation: Some(&coll), ext: &ext, vars: None },
        position: None,
        now: NOW,
        insert: false,
    };
    let d = bytes(r#"{"a": ["b"]}"#);
    let out = parse(r#"{"$max": {"m": "B"}, "$addToSet": {"a": "c"}}"#, None)
        .unwrap()
        .apply(raw(&d), &ctx)
        .unwrap();
    assert_eq!(json(&out.doc), canon(r#"{"a": ["b", "c"], "m": "B"}"#));
}

#[test]
fn upserts_build_the_document_from_the_filter() {
    let ext = TestExt::default();
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position: None,
        now: NOW,
        insert: true,
    };
    let up = |filter: &str, update: &str| -> Result<String, MqlError> {
        let f = bytes(filter);
        let filter = Filter::parse(raw(&f), FilterUse::Query)?;
        let u = parse(update, None)?;
        upsert(&filter, &u, &ctx, || Value::Int32(99)).map(|d| json(&d))
    };
    let cases = [
        (r#"{"a": 1}"#, r#"{"$set": {"b": 2}}"#, r#"{"_id": 99, "a": 1, "b": 2}"#),
        (
            r#"{"_id": 5, "a.b": 1}"#,
            r#"{"$inc": {"n": 1}}"#,
            r#"{"_id": 5, "a": {"b": 1}, "n": 1}"#,
        ),
        (
            r#"{"a": {"$gt": 1}, "b": {"$eq": 2}, "c": {"$in": [3]}}"#,
            r#"{"$set": {"d": 4}}"#,
            r#"{"_id": 99, "b": 2, "c": 3, "d": 4}"#,
        ),
        (
            r#"{"$and": [{"a": 1}, {"$or": [{"b": 2}]}]}"#,
            r#"{"$set": {"z": 1}}"#,
            r#"{"_id": 99, "a": 1, "b": 2, "z": 1}"#,
        ),
        (r#"{"$or": [{"a": 1}, {"b": 2}]}"#, r#"{"$set": {"z": 1}}"#, r#"{"_id": 99, "z": 1}"#),
        (
            r#"{"a": 1}"#,
            r#"{"$setOnInsert": {"s": 1}, "$set": {"a": 2}}"#,
            r#"{"_id": 99, "a": 2, "s": 1}"#,
        ),
        (
            r#"{"a": {"$elemMatch": {"b": 1}}, "r": {"$regularExpression": {"pattern": "x", "options": ""}}}"#,
            r#"{"$set": {"z": 1}}"#,
            r#"{"_id": 99, "z": 1}"#,
        ),
        (
            r#"{"a": {"$eq": 1, "$gt": 0}}"#,
            r#"{"$set": {"z": 1}}"#,
            r#"{"_id": 99, "a": 1, "z": 1}"#,
        ),
        (r#"{"_id": 5}"#, r#"{"x": 1}"#, r#"{"_id": 5, "x": 1}"#),
        (r#"{"a": 1}"#, r#"{"x": 1}"#, r#"{"_id": 99, "x": 1}"#),
        (r#"{"_id": 5}"#, r#"{"x": 1, "_id": 5}"#, r#"{"_id": 5, "x": 1}"#),
        (r#"{"a": 1}"#, r#"{"x": 1, "_id": 7}"#, r#"{"_id": 7, "x": 1}"#),
        (r#"{"a": 1}"#, r#"{"$set": {"_id": 3}}"#, r#"{"_id": 3, "a": 1}"#),
    ];
    for (filter, update, want) in cases {
        assert_eq!(
            up(filter, update).unwrap_or_else(|e| panic!("{filter} {update}: {e}")),
            canon(want),
            "{filter} {update}"
        );
    }
    let code = |filter: &str, update: &str| up(filter, update).unwrap_err().code;
    assert_eq!(
        code(r#"{"a": 1, "a.b": 2}"#, r#"{"$set": {"z": 1}}"#),
        codes::NOT_SINGLE_VALUE_FIELD
    );
    assert_eq!(
        code(r#"{"$and": [{"a": 1}, {"a": 2}]}"#, r#"{"$set": {"z": 1}}"#),
        codes::NOT_SINGLE_VALUE_FIELD
    );
    assert_eq!(code(r#"{"_id": 1}"#, r#"{"$set": {"_id": 2}}"#), codes::IMMUTABLE_FIELD);
    assert_eq!(code(r#"{"_id": 1}"#, r#"{"_id": 2}"#), codes::IMMUTABLE_FIELD);
    assert_eq!(code(r#"{"a": 1}"#, r#"{"$set": {"a.$": 2}}"#), codes::BAD_VALUE);
    assert_eq!(
        up(r#"{"a": 1, "a.b": 2}"#, r#"{"$set": {"z": 1}}"#).unwrap_err().message,
        "cannot infer query fields to set, both paths 'a.b' and 'a' are matched"
    );
    assert_eq!(
        up(r#"{"$and": [{"a": 1}, {"a": 2}]}"#, r#"{"$set": {"z": 1}}"#).unwrap_err().message,
        "cannot infer query fields to set, path 'a' is matched twice"
    );
}

#[test]
fn pipelines_go_to_the_integration_point() {
    let u = parse(r#"[{"$set": {"a": 1}}]"#, None).unwrap();
    assert!(u.is_pipeline());
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: None,
        now: NOW,
        insert: false,
    };
    let d = bytes(r#"{"_id": 1}"#);
    assert_eq!(u.apply(raw(&d), &ctx).unwrap_err().code, codes::INTERNAL_ERROR);
    let ext = TestExt::default();
    let ctx =
        UpdateContext { matcher: MatchContext { collation: None, ext: &ext, vars: None }, ..ctx };
    let out = u.apply(raw(&d), &ctx).unwrap();
    assert_eq!(json(&out.doc), canon(r#"{"_id": 1, "piped": true}"#));
    assert!(out.modified);
}

#[test]
fn errors() {
    let bad = codes::BAD_VALUE;
    let tm = codes::TYPE_MISMATCH;
    let ftp = codes::FAILED_TO_PARSE;
    let cases: &[(&str, Option<&str>, &str, ErrorCode, &str)] = &[
        (
            r#"{"$set": {"_id": 2}}"#,
            None,
            r#"{"_id": 1}"#,
            codes::IMMUTABLE_FIELD,
            "Performing an update on the path '_id' would modify the immutable field '_id'",
        ),
        (
            r#"{"$unset": {"_id": 1}}"#,
            None,
            r#"{"_id": 1}"#,
            codes::IMMUTABLE_FIELD,
            "Performing an update on the path '_id' would modify the immutable field '_id'",
        ),
        (
            r#"{"_id": 2}"#,
            None,
            r#"{"_id": 1}"#,
            codes::IMMUTABLE_FIELD,
            "After applying the update, the (immutable) field '_id' was found to have been altered to _id: 2",
        ),
        (
            r#"{"$set": {"a": 1}, "$inc": {"a": 1}}"#,
            None,
            "{}",
            codes::CONFLICTING_UPDATE_OPERATORS,
            "Updating the path 'a' would create a conflict at 'a'",
        ),
        (
            r#"{"$set": {"a.b": 1, "a": 1}}"#,
            None,
            "{}",
            codes::CONFLICTING_UPDATE_OPERATORS,
            "Updating the path 'a.b' would create a conflict at 'a'",
        ),
        (
            r#"{"$rename": {"a": "b"}, "$set": {"b.c": 1}}"#,
            None,
            "{}",
            codes::CONFLICTING_UPDATE_OPERATORS,
            "Updating the path 'b.c' would create a conflict at 'b'",
        ),
        (
            r#"{"$foo": {"a": 1}}"#,
            None,
            "{}",
            ftp,
            "Unknown modifier: $foo. Expected a valid update modifier or pipeline-style update specified as an array",
        ),
        (
            r#"{"$set": {"a": 1}, "b": 1}"#,
            None,
            "{}",
            ftp,
            "Unknown modifier: b. Expected a valid update modifier or pipeline-style update specified as an array",
        ),
        (
            // Measured: with a field first it is a replacement, checked while it runs.
            r#"{"a": 1, "$set": {"b": 1}}"#,
            None,
            "{}",
            codes::DOLLAR_PREFIXED_FIELD_NAME,
            "The dollar ($) prefixed field '$set' in '$set' is not allowed in the context of an update's replacement document. Consider using an aggregation pipeline with $replaceWith.",
        ),
        (
            r#"{"$set": 1}"#,
            None,
            "{}",
            ftp,
            "Modifiers operate on fields but we found type int instead. For example: {$mod: {<field>: ...}} not {$set: 1}",
        ),
        // `{a: {$x: 1}}` is stored as it is (measured): only the top level is checked.
        (
            r#"{"$set": {"a..b": 1}}"#,
            None,
            "{}",
            codes::EMPTY_FIELD_NAME,
            "The update path 'a..b' contains an empty field name, which is not allowed.",
        ),
        (
            r#"{"$set": {"": 1}}"#,
            None,
            "{}",
            codes::EMPTY_FIELD_NAME,
            "An empty update path is not valid.",
        ),
        (
            r#"{"$set": {"$x": 1}}"#,
            None,
            "{}",
            codes::DOLLAR_PREFIXED_FIELD_NAME,
            "The dollar ($) prefixed field '$x' in '$x' is not allowed in the context of an update's replacement document. Consider using an aggregation pipeline with $replaceWith.",
        ),
        (
            r#"{"$set": {"a.b": 1}}"#,
            None,
            r#"{"a": 5}"#,
            codes::PATH_NOT_VIABLE,
            "Cannot create field 'b' in element {a: 5}",
        ),
        (
            r#"{"$set": {"a.b": 1}}"#,
            None,
            r#"{"a": [1]}"#,
            codes::PATH_NOT_VIABLE,
            "Cannot create field 'b' in element {a: [ 1 ]}",
        ),
        (
            r#"{"$set": {"a.$": 1}}"#,
            None,
            r#"{"a": [1]}"#,
            bad,
            "The positional operator did not find the match needed from the query.",
        ),
        (
            r#"{"$set": {"a.$[]": 1}}"#,
            None,
            r#"{"b": 1}"#,
            bad,
            "The path 'a' must exist in the document in order to apply array updates.",
        ),
        (
            r#"{"$set": {"a.$[]": 1}}"#,
            None,
            r#"{"a": 1}"#,
            bad,
            "Cannot apply array updates to non-array element a: 1",
        ),
        (
            r#"{"$set": {"a.$[x]": 1}}"#,
            None,
            r#"{"a": [1]}"#,
            bad,
            "No array filter found for identifier 'x' in path 'a.$[x]'",
        ),
        (
            r#"{"$set": {"a": 1}}"#,
            Some(r#"[{"x": 1}]"#),
            "{}",
            ftp,
            "The array filter for identifier 'x' was not used in the update { $set: { a: 1 } }",
        ),
        (
            r#"{"$set": {"a.$[x]": 1}}"#,
            Some(r#"[{"x": 1}, {"x": 2}]"#),
            "{}",
            ftp,
            "Found multiple array filters with the same top-level field name x",
        ),
        (
            r#"{"$set": {"a.$[x]": 1}}"#,
            Some(r#"[{"x": 1, "y": 2}]"#),
            "{}",
            ftp,
            "Error parsing array filter :: caused by :: Expected a single top-level field name, found 'x' and 'y'",
        ),
        (
            r#"{"$set": {"a.$[X]": 1}}"#,
            Some(r#"[{"X": 1}]"#),
            "{}",
            bad,
            "Error parsing array filter :: caused by :: The top-level field name must be an alphanumeric string beginning with a lowercase letter, found 'X'",
        ),
        (
            r#"{"$set": {"a.$[x]": 1}}"#,
            Some(r#"[1]"#),
            "{}",
            tm,
            "Each array filter must be an object",
        ),
        (
            r#"{"$set": {"a.$[x]": 1}}"#,
            Some(r#"[{}]"#),
            "{}",
            ftp,
            "Cannot use an expression without a top-level field name in arrayFilters",
        ),
        (
            r#"{"$inc": {"a": "x"}}"#,
            None,
            "{}",
            tm,
            "Cannot increment with non-numeric argument: {a: \"x\"}",
        ),
        (
            r#"{"$inc": {"a": 1}}"#,
            None,
            r#"{"_id": 1, "a": "x"}"#,
            tm,
            "Cannot apply $inc to a value of non-numeric type. {_id: 1} has the field 'a' of non-numeric type string",
        ),
        (
            r#"{"$mul": {"a": "x"}}"#,
            None,
            "{}",
            tm,
            "Cannot multiply with non-numeric argument: {a: \"x\"}",
        ),
        (
            r#"{"$inc": {"a": {"$numberLong": "1"}}}"#,
            None,
            r#"{"_id": 1, "a": {"$numberLong": "9223372036854775807"}}"#,
            bad,
            "Failed to apply $inc operations to current value ((NumberLong)9223372036854775807) for document {_id: 1}",
        ),
        (
            r#"{"$mul": {"a": 2}}"#,
            None,
            r#"{"_id": 1, "a": {"$numberLong": "9223372036854775807"}}"#,
            bad,
            "Failed to apply $mul operations to current value ((NumberLong)9223372036854775807) for document {_id: 1}",
        ),
        (
            r#"{"$rename": {"a": 1}}"#,
            None,
            "{}",
            bad,
            "The 'to' field for $rename must be a string: a: 1",
        ),
        (
            r#"{"$rename": {"a": "a"}}"#,
            None,
            "{}",
            bad,
            "The source and target field for $rename must differ: a: \"a\"",
        ),
        (
            r#"{"$rename": {"a": "a.b"}}"#,
            None,
            "{}",
            bad,
            "The source and target field for $rename must not be on the same path: a: \"a.b\"",
        ),
        (
            r#"{"$rename": {"a.$": "b"}}"#,
            None,
            "{}",
            bad,
            "The source field for $rename may not be dynamic: a.$",
        ),
        (
            r#"{"$rename": {"a": "b.$"}}"#,
            None,
            "{}",
            bad,
            "The destination field for $rename may not be dynamic: b.$",
        ),
        (
            r#"{"$rename": {"a.0": "b"}}"#,
            None,
            r#"{"_id": {"k": "v"}, "a": [1]}"#,
            bad,
            "The source field cannot be an array element, 'a.0' in doc with _id: { k: \"v\" } has an array field called 'a'",
        ),
        (
            r#"{"$rename": {"a": "b.0"}}"#,
            None,
            r#"{"_id": 2, "a": 1, "b": [1]}"#,
            bad,
            "The destination field cannot be an array element, 'b.0' in doc with _id: 2 has an array field called 'b'",
        ),
        (
            r#"{"$currentDate": {"d": 1}}"#,
            None,
            "{}",
            bad,
            "int is not valid type for $currentDate. Please use a boolean ('true') or a $type expression ({$type: 'timestamp/date'}).",
        ),
        (
            r#"{"$currentDate": {"d": {"$type": "x"}}}"#,
            None,
            "{}",
            bad,
            "The '$type' string field is required to be 'date' or 'timestamp': {$currentDate: {field : {$type: 'date'}}}",
        ),
        (
            r#"{"$currentDate": {"d": {"x": 1}}}"#,
            None,
            "{}",
            bad,
            "Unrecognized $currentDate option: x",
        ),
        (
            r#"{"$addToSet": {"a": 1}}"#,
            None,
            r#"{"a": 1}"#,
            bad,
            "Cannot apply $addToSet to non-array field. Field named 'a' has non-array type int",
        ),
        (
            r#"{"$addToSet": {"a": {"$each": 1}}}"#,
            None,
            "{}",
            tm,
            "The argument to $each in $addToSet must be an array but it was of type int",
        ),
        (
            r#"{"$addToSet": {"a": {"$each": [], "x": 1}}}"#,
            None,
            "{}",
            bad,
            "Found unexpected fields after $each in $addToSet: { $each: [], x: 1 }",
        ),
        (r#"{"$pop": {"a": 2}}"#, None, "{}", ftp, "$pop expects 1 or -1, found: 2"),
        (
            r#"{"$pop": {"a": 1}}"#,
            None,
            r#"{"a": 1}"#,
            tm,
            "Path 'a' contains an element of non-array type 'int'",
        ),
        (
            r#"{"$pull": {"a": 1}}"#,
            None,
            r#"{"a": 1}"#,
            bad,
            "Cannot apply $pull to a non-array value",
        ),
        (
            r#"{"$pullAll": {"a": 1}}"#,
            None,
            "{}",
            bad,
            "$pullAll requires an array argument but was given a int",
        ),
        (
            r#"{"$pullAll": {"a": [1]}}"#,
            None,
            r#"{"a": 1}"#,
            bad,
            "Cannot apply $pull to a non-array value",
        ),
        (
            r#"{"$push": {"a": 1}}"#,
            None,
            r#"{"_id": 1, "a": 1}"#,
            bad,
            "The field 'a' must be an array but is of type int in document {_id: 1}",
        ),
        (
            r#"{"$push": {"a": {"$each": 1}}}"#,
            None,
            "{}",
            bad,
            "The argument to $each in $push must be an array but it was of type: int",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$x": 1}}}"#,
            None,
            "{}",
            bad,
            "Unrecognized clause in $push: $x",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$slice": "x"}}}"#,
            None,
            "{}",
            bad,
            "The value for $slice must be an integer value but was given type: string",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$position": 1.5}}}"#,
            None,
            "{}",
            bad,
            "The value for $position must be an integer value, not of type: double",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$sort": 2}}}"#,
            None,
            "{}",
            bad,
            "The $sort element value must be either 1 or -1",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$sort": {}}}}"#,
            None,
            "{}",
            bad,
            "The sort pattern is empty when it should be a set of fields.",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$sort": {"b": 2}}}}"#,
            None,
            "{}",
            bad,
            "The $sort element value must be either 1 or -1",
        ),
        (
            r#"{"$push": {"a": {"$each": [], "$sort": {"": 1}}}}"#,
            None,
            "{}",
            bad,
            "The $sort field is a dotted field but has an empty part: ",
        ),
        (
            r#"{"$bit": {"a": 1}}"#,
            None,
            "{}",
            bad,
            "The $bit modifier is not compatible with a int. You must pass in an embedded document: {$bit: {field: {and/or/xor: #}}",
        ),
        (
            r#"{"$bit": {"a": {"nand": 1}}}"#,
            None,
            "{}",
            bad,
            "The $bit modifier only supports 'and', 'or', and 'xor', not 'nand' which is an unknown operator: {nand: 1}",
        ),
        (
            r#"{"$bit": {"a": {"and": 1.0}}}"#,
            None,
            "{}",
            bad,
            "The $bit modifier field must be an Integer(32/64 bit); a 'double' is not supported here: {and: 1.0}",
        ),
        (
            r#"{"$bit": {"a": {}}}"#,
            None,
            "{}",
            bad,
            "You must pass in at least one bitwise operation. The format is: {$bit: {field: {and/or/xor: #}}",
        ),
        (
            r#"{"$bit": {"a": {"and": 1}}}"#,
            None,
            r#"{"_id": 7, "a": "x"}"#,
            bad,
            "Cannot apply $bit to a value of non-integral type._id: 7 has the field a of non-integer type string",
        ),
    ];
    for &(update, filters, doc, code, message) in cases {
        assert_eq!(error(update, filters, doc), (code, message.to_owned()), "{update} on {doc}");
    }
    assert!(parse("1", None).is_err());
}

#[test]
fn documents_over_16_mib_are_the_servers_to_refuse() {
    // The update itself goes past 16 MiB: the server refuses to store it with RefDb's words for
    // an update or an upsert (003 T009, teseladb-wire `stored_bytes`).
    let big = "x".repeat(9 * 1024 * 1024);
    let doc = format!(r#"{{"a": "{big}"}}"#);
    let update = parse(&format!(r#"{{"$set": {{"b": "{big}"}}}}"#), None).unwrap();
    let ext = TestExt::default();
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position: None,
        now: NOW,
        insert: false,
    };
    let d = bytes(&doc);
    let out = update.apply(raw(&d), &ctx).unwrap();
    assert!(out.modified);
    let e = out.doc.to_bytes().unwrap_err();
    assert_eq!(e.code(), (codes::BSON_OBJECT_TOO_LARGE.code, codes::BSON_OBJECT_TOO_LARGE.name));
}

/// A positional segment can be any element, so it meets a position of the same array (measured
/// on a `refrouter` 9.0.4).
#[test]
fn positional_paths_conflict_with_positions() {
    let u = |spec: &str| {
        let b = common::bytes(spec);
        teseladb_mql::update::Update::parse(common::raw(&b).get("u").unwrap(), None).map(|_| ())
    };
    let e = u(r#"{"u": {"$set": {"docs.$[].x": 1, "docs.0.x": 2}}}"#).unwrap_err();
    assert_eq!(e.message, "Updating the path 'docs.0.x' would create a conflict at 'docs'");
    assert!(u(r#"{"u": {"$set": {"docs.$[].x": 1, "other.0.x": 2}}}"#).is_ok());
    assert!(u(r#"{"u": {"$set": {"docs.$[].x": 1, "docs.$[].y": 2}}}"#).is_ok());
}

/// What each operator does to its path, for the change description (014 FR-009).
#[test]
fn each_operator_says_what_it_does_to_its_path() {
    use teseladb_mql::update::PathChange::*;
    let u = parse(
        r#"{"$set": {"a": 1}, "$setOnInsert": {"i": 1}, "$unset": {"m": ""}, "$inc": {"n": 1},
            "$rename": {"r": "s"}, "$push": {"p": 1, "q": {"$each": [1], "$sort": 1}},
            "$addToSet": {"t": 1}, "$pop": {"o": 1}, "$pull": {"l": 1}, "$pullAll": {"k": [1]},
            "$min": {"x": 1}, "$currentDate": {"d": true}, "$bit": {"b": {"and": 1}}}"#,
        None,
    )
    .unwrap();
    let changes = u.path_changes(false).unwrap();
    let find = |p: &str| changes.iter().find(|(_, q)| q == p).map(|(c, _)| c.clone());
    for (path, want) in [
        ("a", Some(Assign)),
        ("i", None),
        ("m", Some(Unset)),
        ("n", Some(Assign)),
        ("r", Some(Rename("s".into()))),
        ("p", Some(Append)),
        ("q", Some(Rewrite)),
        ("t", Some(Append)),
        ("o", Some(Rewrite)),
        ("l", Some(Rewrite)),
        ("k", Some(Rewrite)),
        ("x", Some(Assign)),
        ("d", Some(Assign)),
        ("b", Some(Assign)),
    ] {
        assert_eq!(find(path), want, "{path}");
    }
    assert!(u.path_changes(true).unwrap().iter().any(|(c, p)| *c == Assign && p == "i"));
    assert_eq!(parse(r#"{"a": 1}"#, None).unwrap().path_changes(false), None);
    assert_eq!(parse(r#"[{"$set": {"a": 1}}]"#, None).unwrap().path_changes(false), None);
}
