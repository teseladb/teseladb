//! Projections of `find` (006 T018, FR-011): inclusion and exclusion, `_id`, dotted paths through
//! arrays, `$`, `$elemMatch`, `$slice`, `$meta` and expressions, with the reference server's
//! errors.

pub mod common;

use common::{TestExt, bytes, canon, json, raw};
use teseladb_mql::projection::{
    POSITIONAL_MISMATCH, POSITIONAL_NO_MATCH, Projection, ProjectionContext,
};
use teseladb_mql::{MatchContext, NoExtensions, codes};

fn project_at(spec: &str, doc: &str, position: Option<usize>) -> String {
    let ext = TestExt::default();
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position,
        sort_key: None,
        index_key: None,
    };
    let s = bytes(spec);
    let d = bytes(doc);
    let p = Projection::parse(raw(&s)).unwrap_or_else(|e| panic!("{spec}: {e}"));
    json(&p.apply(raw(&d), &ctx).unwrap_or_else(|e| panic!("{spec} on {doc}: {e}")))
}

/// `(projection, document, result)`.
fn check(cases: &[(&str, &str, &str)]) {
    for &(spec, doc, want) in cases {
        assert_eq!(project_at(spec, doc, None), canon(want), "{spec} on {doc}");
    }
}

fn error(spec: &str) -> (i32, String) {
    let s = bytes(spec);
    let e = Projection::parse(raw(&s)).expect_err(spec);
    (e.code.code, e.message)
}

#[test]
fn inclusion_and_exclusion() {
    let d = r#"{"_id": 1, "a": 1, "b": {"c": 2, "d": 3}, "e": [{"c": 1, "x": 0}, 5, {"x": 2}]}"#;
    check(&[
        ("{}", d, d),
        (r#"{"a": 1}"#, d, r#"{"_id": 1, "a": 1}"#),
        (r#"{"a": true}"#, d, r#"{"_id": 1, "a": 1}"#),
        (r#"{"a": 2.5}"#, d, r#"{"_id": 1, "a": 1}"#),
        (r#"{"a": 1, "_id": 0}"#, d, r#"{"a": 1}"#),
        (
            r#"{"_id": 0}"#,
            d,
            r#"{"a": 1, "b": {"c": 2, "d": 3}, "e": [{"c": 1, "x": 0}, 5, {"x": 2}]}"#,
        ),
        (r#"{"_id": 1}"#, d, r#"{"_id": 1}"#),
        (
            r#"{"a": 0}"#,
            d,
            r#"{"_id": 1, "b": {"c": 2, "d": 3}, "e": [{"c": 1, "x": 0}, 5, {"x": 2}]}"#,
        ),
        (r#"{"a": false, "b": 0}"#, d, r#"{"_id": 1, "e": [{"c": 1, "x": 0}, 5, {"x": 2}]}"#),
        (r#"{"b.c": 1}"#, d, r#"{"_id": 1, "b": {"c": 2}}"#),
        (r#"{"b": {"c": 1}}"#, d, r#"{"_id": 1, "b": {"c": 2}}"#),
        (
            r#"{"b.c": 0}"#,
            d,
            r#"{"_id": 1, "a": 1, "b": {"d": 3}, "e": [{"c": 1, "x": 0}, 5, {"x": 2}]}"#,
        ),
        (r#"{"e.c": 1}"#, d, r#"{"_id": 1, "e": [{"c": 1}, {}]}"#),
        (
            r#"{"e.c": 0}"#,
            d,
            r#"{"_id": 1, "a": 1, "b": {"c": 2, "d": 3}, "e": [{"x": 0}, 5, {"x": 2}]}"#,
        ),
        (r#"{"z": 1}"#, d, r#"{"_id": 1}"#),
        (r#"{"a.z": 1}"#, d, r#"{"_id": 1}"#),
        (r#"{"b": 1, "a": 1}"#, d, r#"{"_id": 1, "a": 1, "b": {"c": 2, "d": 3}}"#),
        (r#"{"x.y": 1}"#, r#"{"x": [[{"y": 1}], {"y": 2}]}"#, r#"{"x": [[{"y": 1}], {"y": 2}]}"#),
        (r#"{"x.y": 0}"#, r#"{"x": [[{"y": 1, "z": 1}], 3]}"#, r#"{"x": [[{"z": 1}], 3]}"#),
    ]);
}

#[test]
fn slice() {
    let d = r#"{"_id": 1, "a": [1, 2, 3, 4, 5], "b": 7, "c": {"d": [1, 2, 3]}}"#;
    check(&[
        (r#"{"a": {"$slice": 2}}"#, d, r#"{"_id": 1, "a": [1, 2], "b": 7, "c": {"d": [1, 2, 3]}}"#),
        (
            r#"{"a": {"$slice": -2}}"#,
            d,
            r#"{"_id": 1, "a": [4, 5], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
        (
            r#"{"a": {"$slice": [1, 2]}}"#,
            d,
            r#"{"_id": 1, "a": [2, 3], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
        (
            r#"{"a": {"$slice": [-2, 5]}}"#,
            d,
            r#"{"_id": 1, "a": [4, 5], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
        (
            r#"{"a": {"$slice": [-9, 2]}}"#,
            d,
            r#"{"_id": 1, "a": [1, 2], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
        (
            r#"{"a": {"$slice": [9, 2]}}"#,
            d,
            r#"{"_id": 1, "a": [], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
        (r#"{"a": {"$slice": 0}}"#, d, r#"{"_id": 1, "a": [], "b": 7, "c": {"d": [1, 2, 3]}}"#),
        (
            r#"{"a": {"$slice": -9}}"#,
            d,
            r#"{"_id": 1, "a": [1, 2, 3, 4, 5], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
        (r#"{"b": {"$slice": 1}}"#, d, d),
        (r#"{"a": {"$slice": 1}, "b": 1}"#, d, r#"{"_id": 1, "a": [1], "b": 7}"#),
        (r#"{"a": {"$slice": 1}, "b": 0}"#, d, r#"{"_id": 1, "a": [1], "c": {"d": [1, 2, 3]}}"#),
        (
            r#"{"c.d": {"$slice": 1}}"#,
            d,
            r#"{"_id": 1, "a": [1, 2, 3, 4, 5], "b": 7, "c": {"d": [1]}}"#,
        ),
        (
            r#"{"a": {"$slice": {"$numberLong": "1"}}}"#,
            d,
            r#"{"_id": 1, "a": [1], "b": 7, "c": {"d": [1, 2, 3]}}"#,
        ),
    ]);
}

#[test]
fn elem_match() {
    let d =
        r#"{"_id": 1, "s": [{"k": 1, "v": "a"}, {"k": 2, "v": "b"}, {"k": 2, "v": "c"}], "t": 3}"#;
    check(&[
        (r#"{"s": {"$elemMatch": {"k": 2}}}"#, d, r#"{"_id": 1, "s": [{"k": 2, "v": "b"}]}"#),
        (r#"{"s": {"$elemMatch": {"k": 9}}}"#, d, r#"{"_id": 1}"#),
        (
            // Measured: in an inclusion `$elemMatch` comes after the included fields; in an
            // exclusion it keeps its place.
            r#"{"s": {"$elemMatch": {"k": 2}}, "t": 1}"#,
            d,
            r#"{"_id": 1, "t": 3, "s": [{"k": 2, "v": "b"}]}"#,
        ),
        (
            r#"{"s": {"$elemMatch": {"k": 2}}, "t": 0}"#,
            d,
            r#"{"_id": 1, "s": [{"k": 2, "v": "b"}]}"#,
        ),
        (r#"{"s": {"$elemMatch": {"k": 9}}, "t": 0}"#, d, r#"{"_id": 1}"#),
        (
            r#"{"s": {"$elemMatch": {"k": 2}}, "_id": 0, "t": {"$slice": 1}}"#,
            d,
            r#"{"s": [{"k": 2, "v": "b"}], "t": 3}"#,
        ),
        (r#"{"t": {"$elemMatch": {"k": 2}}}"#, d, r#"{"_id": 1}"#),
        (r#"{"s": {"$elemMatch": {"k": 2}}, "_id": 0}"#, d, r#"{"s": [{"k": 2, "v": "b"}]}"#),
    ]);
}

#[test]
fn positional() {
    let d = r#"{"_id": 1, "g": [80, 90, 95], "n": 1, "o": {"g": [1, 2]}}"#;
    assert_eq!(project_at(r#"{"g.$": 1}"#, d, Some(1)), canon(r#"{"_id": 1, "g": [90]}"#));
    assert_eq!(
        project_at(r#"{"g.$": 1, "n": 1}"#, d, Some(2)),
        canon(r#"{"_id": 1, "g": [95], "n": 1}"#)
    );
    assert_eq!(project_at(r#"{"o.g.$": 1}"#, d, Some(0)), canon(r#"{"_id": 1, "o": {"g": [1]}}"#));
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: None,
        sort_key: None,
        index_key: None,
    };
    let s = bytes(r#"{"g.$": 1}"#);
    let doc = bytes(d);
    let e = Projection::parse(raw(&s)).unwrap().apply(raw(&doc), &ctx).unwrap_err();
    assert_eq!(e.code, POSITIONAL_NO_MATCH);
    let ctx = ProjectionContext { position: Some(7), ..ctx };
    let e = Projection::parse(raw(&s)).unwrap().apply(raw(&doc), &ctx).unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (POSITIONAL_MISMATCH, "positional operator '.$' element mismatch")
    );
    // A value that is not an array is projected as it is; a missing one is left out.
    let ctx = ProjectionContext { position: Some(0), ..ctx };
    assert_eq!(project_at(r#"{"n.$": 1}"#, d, Some(0)), canon(r#"{"_id": 1, "n": 1}"#));
    assert_eq!(project_at(r#"{"z.$": 1}"#, d, Some(0)), canon(r#"{"_id": 1}"#));
    // `a.b.$`: the element of the first array on the path, with `b` projected inside it.
    let arr = r#"{"_id": 2, "a": [{"b": 1, "c": "x"}, {"b": 2, "c": "y"}]}"#;
    assert_eq!(
        project_at(r#"{"a.b.$": 1}"#, arr, Some(1)),
        canon(r#"{"_id": 2, "a": [{"b": 2}]}"#)
    );
    let nested = r#"{"_id": 3, "a": [[1, 2], [3]]}"#;
    assert_eq!(project_at(r#"{"a.b.$": 1}"#, nested, Some(1)), canon(r#"{"_id": 3, "a": [[]]}"#));
    // Measured: an element that is not a document is a mismatch; an array inside is whole.
    let s = bytes(r#"{"a.b.$": 1}"#);
    let scalar = bytes(r#"{"_id": 4, "a": [5]}"#);
    let at0 = ProjectionContext { position: Some(0), ..ctx };
    let e = Projection::parse(raw(&s)).unwrap().apply(raw(&scalar), &at0).unwrap_err();
    assert_eq!(e.code, POSITIONAL_MISMATCH);
    assert_eq!(
        project_at(r#"{"a.b.$": 1}"#, r#"{"_id": 5, "a": [{"c": 1}]}"#, Some(0)),
        canon(r#"{"_id": 5, "a": [{}]}"#)
    );
    assert_eq!(
        project_at(r#"{"a.b.$": 1}"#, r#"{"_id": 6, "a": [{"b": [7, 8]}]}"#, Some(0)),
        canon(r#"{"_id": 6, "a": [{"b": [7, 8]}]}"#)
    );
    let _ = ctx;
    assert!(Projection::parse(raw(&bytes(r#"{"g.$": 1}"#))).unwrap().needs_position());
    assert!(!Projection::parse(raw(&bytes(r#"{"g": 1}"#))).unwrap().needs_position());
}

#[test]
fn meta_and_expressions() {
    let d = r#"{"_id": 1, "a": 2, "b": {"c": 3}}"#;
    check(&[
        (
            r#"{"score": {"$meta": "textScore"}}"#,
            d,
            r#"{"_id": 1, "a": 2, "b": {"c": 3}, "score": 1.5}"#,
        ),
        (r#"{"score": {"$meta": "textScore"}, "a": 1}"#, d, r#"{"_id": 1, "a": 2, "score": 1.5}"#),
        (r#"{"k": {"$meta": "indexKey"}, "a": 1}"#, d, r#"{"_id": 1, "a": 2}"#),
        (r#"{"x": "$a"}"#, d, r#"{"_id": 1, "x": 2}"#),
        (r#"{"x": "lit", "a": 1}"#, d, r#"{"_id": 1, "a": 2, "x": "lit"}"#),
        (r#"{"a": "$b"}"#, d, r#"{"_id": 1, "a": {"c": 3}}"#),
        (r#"{"x": {"$literal": 5}}"#, d, r#"{"_id": 1, "x": {"$literal": 5}}"#),
        (r#"{"x": [1, "$a"]}"#, d, r#"{"_id": 1, "x": [1, "$a"]}"#),
        (r#"{"b.x": "$a"}"#, d, r#"{"_id": 1, "b": {"x": 2}}"#),
        (r#"{"_id": "$a"}"#, d, r#"{"_id": 2}"#),
    ]);
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: None,
        sort_key: None,
        index_key: None,
    };
    let s = bytes(r#"{"x": "$a"}"#);
    let doc = bytes(d);
    assert_eq!(
        Projection::parse(raw(&s)).unwrap().apply(raw(&doc), &ctx).unwrap_err().code,
        codes::INTERNAL_ERROR
    );
    let ext = TestExt::default();
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position: None,
        sort_key: None,
        index_key: None,
    };
    let s = bytes(r#"{"x": {"$fail": 1}}"#);
    assert!(Projection::parse(raw(&s)).unwrap().apply(raw(&doc), &ctx).is_err());
}

#[test]
fn malformed_projections() {
    let cases: &[(&str, i32, &str)] = &[
        (r#"{"a": 1, "b": 0}"#, 31254, "Cannot do exclusion on field b in inclusion projection"),
        (r#"{"a": 0, "b": 1}"#, 31253, "Cannot do inclusion on field b in exclusion projection"),
        (r#"{"a": 0, "b": "$x"}"#, 31253, "Cannot do inclusion on field b in exclusion projection"),
        (r#"{"a": 1, "a.b": 1}"#, 31249, "Path collision at a.b remaining portion b"),
        (r#"{"a.b": 1, "a": 1}"#, 31250, "Path collision at a"),
        (
            r#"{"a.$": 1, "b.$": 1}"#,
            31276,
            "Cannot specify more than one positional projection per query.",
        ),
        (
            r#"{"a.$.b": 1}"#,
            31394,
            "As of 4.4, it's illegal to specify positional operator in the middle of a path.Positional projection may only be used at the end, for example: a.b.$. If the query previously used a form like a.b.$.d, remove the parts following the '$' and the results will be equivalent.",
        ),
        (r#"{"a.$": 0}"#, 31395, "positional projection cannot be used with exclusion"),
        (
            r#"{"a": {"$slice": "x"}}"#,
            28667,
            "Invalid $slice syntax. The given syntax { $slice: \"x\" } did not match the find() syntax because :: Location31273: $slice only supports numbers and [skip, limit] arrays :: The given syntax did not match the expression $slice syntax. :: caused by :: Expression $slice takes at least 2 arguments, and at most 3, but 1 were passed in.",
        ),
        (
            r#"{"a": {"$slice": [1]}}"#,
            28667,
            "Invalid $slice syntax. The given syntax { $slice: [ 1 ] } did not match the find() syntax because :: Location31272: $slice array argument should be of form [skip, limit] :: The given syntax did not match the expression $slice syntax. :: caused by :: Expression $slice takes at least 2 arguments, and at most 3, but 1 were passed in.",
        ),
        (
            r#"{"a": {"$slice": [1, 2, 3, 4]}}"#,
            28667,
            "Invalid $slice syntax. The given syntax { $slice: [ 1, 2, 3, 4 ] } did not match the find() syntax because :: Location31272: $slice array argument should be of form [skip, limit] :: The given syntax did not match the expression $slice syntax. :: caused by :: Expression $slice takes at least 2 arguments, and at most 3, but 4 were passed in.",
        ),
        (
            r#"{"a.$": 1, "a": {"$slice": 1}}"#,
            28667,
            "Invalid $slice syntax. The given syntax { $slice: 1 } did not match the find() syntax because :: Location31250: Path collision at a :: The given syntax did not match the expression $slice syntax. :: caused by :: Expression $slice takes at least 2 arguments, and at most 3, but 1 were passed in.",
        ),
        (r#"{"a": {"$slice": 1}, "a.b": 1}"#, 31249, "Path collision at a.b remaining portion b"),
        (
            r#"{"a": {"$elemMatch": {"b": 1}}, "a.b": 1}"#,
            31249,
            "Path collision at a.b remaining portion b",
        ),
        (
            r#"{"a.b": {"$elemMatch": {"x": 1}}}"#,
            31275,
            "Cannot use $elemMatch projection on a nested field.",
        ),
        (
            r#"{"a": {"b": {"$elemMatch": {"c": 1}}}}"#,
            31275,
            "Cannot use $elemMatch projection on a nested field.",
        ),
        (
            r#"{"a": {"$elemMatch": 1}}"#,
            31274,
            "elemMatch: Invalid argument, object required, but got int",
        ),
        (
            r#"{"a.$": 1, "b": {"$elemMatch": {"c": 1}}}"#,
            31255,
            "Cannot specify positional operator and $elemMatch.",
        ),
        (
            r#"{"b": {"$elemMatch": {"c": 1}}, "a.$": 1}"#,
            31256,
            "Cannot specify positional operator and $elemMatch.",
        ),
        (
            r#"{"a.$": {}}"#,
            31271,
            "positional projection cannot be used with an expression or sub object",
        ),
        (r#"{"a.$": null}"#, 31308, "positional projection cannot be used with a literal"),
        (r#"{"a": {"$meta": "nope"}}"#, 17308, "Unsupported $meta field: nope"),
        (r#"{"a": {"$meta": 1}}"#, 17307, "$meta only supports string arguments"),
        (r#"{"a": {}}"#, 51270, "Invalid empty sub-projection: a"),
        (r#"{"a": {"x": {}}}"#, 51270, "Invalid empty sub-projection: x"),
        (
            r#"{"a": {"x": {"y": 1, "z": 0}}}"#,
            31254,
            "Cannot do exclusion on field z in inclusion projection",
        ),
        (
            r#"{"$a": 1}"#,
            16410,
            "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '$a'.",
        ),
        (
            r#"{"$": 1}"#,
            16410,
            "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '$'.",
        ),
        (
            r#"{"a": {"$elemMatch": {"b": 1}, "$slice": 1}}"#,
            16410,
            "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '$elemMatch'.",
        ),
        (
            r#"{"a..b": 1}"#,
            15998,
            "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not be empty strings.",
        ),
        (r#"{"": 1}"#, 40352, "FieldPath cannot be constructed with empty string"),
        (r#"{"a.": 1}"#, 40353, "FieldPath must not end with a '.'."),
    ];
    for &(spec, code, message) in cases {
        assert_eq!(error(spec), (code, message.to_owned()), "{spec}");
    }
}

/// `{$meta: "sortKey"}` gives the values the document was sorted by, also on a dotted path,
/// which goes into each element of an array (measured on a reference router 9.0.4); the `$meta` the
/// query cannot give are refused.
#[test]
fn sort_key_and_metadata_checks() {
    use teseladb_bson::Value;
    let key = [Value::from(1), Value::Null];
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: None,
        sort_key: Some(&key),
        index_key: None,
    };
    let apply = |spec: &str, doc: &str| {
        let (s, d) = (bytes(spec), bytes(doc));
        json(&Projection::parse(raw(&s)).unwrap().apply(raw(&d), &ctx).unwrap())
    };
    assert_eq!(
        apply(r#"{"k": {"$meta": "sortKey"}}"#, r#"{"_id": 1}"#),
        canon(r#"{"_id": 1, "k": [1, null]}"#)
    );
    assert_eq!(
        apply(r#"{"a.k": {"$meta": "sortKey"}}"#, r#"{"_id": 1, "a": [{"b": 1}, 7, [], [2]]}"#),
        canon(
            r#"{"_id": 1, "a": [{"b": 1, "k": [1, null]}, {"k": [1, null]}, [], [{"k": [1, null]}]]}"#
        )
    );
    let check = |spec: &str, text: bool, near: bool, sorted: bool| {
        let s = bytes(spec);
        Projection::parse(raw(&s))
            .unwrap()
            .check_metadata(text, near, sorted)
            .map_err(|e| (e.code.code, e.message))
    };
    let unavailable = |what: &str| {
        Err((40218, format!("query requires {what} metadata, but it is not available")))
    };
    assert_eq!(
        check(r#"{"s": {"$meta": "textScore"}}"#, false, false, true),
        unavailable("text score")
    );
    assert_eq!(check(r#"{"s": {"$meta": "textScore"}}"#, true, false, true), Ok(()));
    assert_eq!(
        check(
            r#"{"s": {"$meta": "textScore"}, "t": {"$meta": "searchScore"}}"#,
            false,
            false,
            true
        ),
        unavailable("$search score")
    );
    assert_eq!(
        check(r#"{"s": {"$meta": "vectorSearchScore"}}"#, true, true, true),
        unavailable("$vectorSearch score")
    );
    assert_eq!(check(r#"{"s": {"$meta": "score"}}"#, true, true, true), unavailable("score"));
    assert_eq!(
        check(r#"{"s": {"$meta": "scoreDetails"}}"#, true, true, true),
        unavailable("scoreDetails")
    );
    assert_eq!(
        check(r#"{"s": {"$meta": "geoNearDistance"}}"#, true, false, true),
        unavailable("$geoNear distance")
    );
    assert_eq!(
        check(r#"{"s": {"$meta": "geoNearPoint"}}"#, true, false, true),
        unavailable("$geoNear point")
    );
    assert_eq!(check(r#"{"s": {"$meta": "geoNearPoint"}}"#, false, true, false), Ok(()));
    assert_eq!(
        check(r#"{"s": {"$meta": "sortKey"}}"#, false, false, false),
        Err((2, "cannot use sortKey $meta projection without a sort".to_owned()))
    );
    assert_eq!(check(r#"{"s": {"$meta": "randVal"}}"#, false, false, false), Ok(()));
}
