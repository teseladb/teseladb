//! `$geoWithin` and `$geoIntersects` without an index (009 T022, FR-012): the documents and the
//! queries below were run on a `refnode` 9.0.4 (30-10-2026) and each query selects the `_id`s it
//! selected there; the errors are its errors.

use teseladb_bson::{RawDoc, extjson};
use teseladb_index::geo::query::{GeoOperator, matches, parse_query};

/// The documents, as extended JSON: their `p`.
const DOCS: [(i32, &str); 17] = [
    (1, "[1, 1]"),
    (2, r#"{"x": 1, "y": 1}"#),
    (3, r#"{"type": "Point", "coordinates": [1, 1]}"#),
    (4, "[[1, 1], [50, 50]]"),
    (5, r#"[{"type": "Point", "coordinates": [1, 1]}]"#),
    (6, r#""x""#),
    (7, "[1]"),
    (8, r#"{"type": "LineString", "coordinates": [[0, 0], [2, 2]]}"#),
    (9, "[200, 1]"),
    (10, r#"{"type": "Point", "coordinates": [50, 50]}"#),
    (12, "[1, 1, 5]"),
    (13, r#"{"a": {"b": 1}, "c": 2}"#),
    (14, r#"[["1", "1"]]"#),
    (15, r#"{"type": "Polygon", "coordinates": [[[0, 0], [3, 0], [3, 3], [0, 3], [0, 0]]]}"#),
    (16, "[true, 1]"),
    (17, r#"[{"$numberDecimal": "1"}, {"$numberLong": "1"}]"#),
    (18, "null"),
];

const SQUARE: &str =
    r#"{"type": "Polygon", "coordinates": [[[0, 0], [5, 0], [5, 5], [0, 5], [0, 0]]]}"#;

fn wrap(json: &str) -> Vec<u8> {
    extjson::parse(&format!(r#"{{"v": {json}}}"#)).unwrap().to_bytes().unwrap()
}

fn selected(op: GeoOperator, spec: &str) -> Vec<i32> {
    let spec = wrap(spec);
    let spec = RawDoc::from_bytes(&spec).unwrap().get("v").unwrap();
    let shape = parse_query(op, spec).unwrap_or_else(|e| panic!("{spec:?}: {e:?}"));
    DOCS.iter()
        .filter(|(_, p)| {
            let bytes = wrap(p);
            matches(&shape, RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap())
        })
        .map(|(id, _)| *id)
        .collect()
}

fn error(op: GeoOperator, spec: &str) -> (i32, String) {
    let spec = wrap(spec);
    let spec = RawDoc::from_bytes(&spec).unwrap().get("v").unwrap();
    let e = parse_query(op, spec).expect_err("refused");
    (e.code, e.message)
}

#[test]
fn each_shape_selects_what_refdb_selected() {
    use GeoOperator::{Intersects, Within};
    let planar = vec![1, 2, 3, 4, 5, 12, 17];
    assert_eq!(
        selected(Within, &format!(r#"{{"$geometry": {SQUARE}}}"#)),
        [1, 2, 3, 4, 5, 8, 12, 17]
    );
    assert_eq!(
        selected(Intersects, &format!(r#"{{"$geometry": {SQUARE}}}"#)),
        [1, 2, 3, 4, 5, 8, 12, 15, 17]
    );
    assert_eq!(selected(Within, r#"{"$box": [[0, 0], [5, 5]]}"#), planar);
    assert_eq!(selected(Within, r#"{"$polygon": [[0, 0], [5, 0], [5, 5], [0, 5]]}"#), planar);
    assert_eq!(selected(Within, r#"{"$center": [[0, 0], 2]}"#), planar);
    assert_eq!(selected(Within, r#"{"$centerSphere": [[0, 0], 0.03]}"#), planar);
    assert_eq!(
        selected(Intersects, r#"{"$geometry": {"type": "Point", "coordinates": [1, 1]}}"#),
        [1, 2, 3, 4, 5, 12, 15, 17]
    );
}

#[test]
fn a_bad_shape_is_refused_as_refdb_refuses_it() {
    use GeoOperator::{Intersects, Within};
    let cases = [
        (
            Within,
            r#"{"$box": [[0, 0]]}"#,
            "Point must be an array or object, instead got type missing",
        ),
        (Within, "{}", "geo query doesn't have any geometry"),
        (
            Within,
            r#"{"$geometry": {"type": "Point", "coordinates": [1, 1]}}"#,
            "$within not supported with provided geometry: { $geoWithin: { $geometry: { type: \"Point\", coordinates: [ 1, 1 ] } } }",
        ),
        (
            Within,
            r#"{"$geometry": {"type": "Point", "coordinates": [1, 1]}, "$box": [[0, 0], [1, 1]]}"#,
            "$geoWithin doesn't accept multiple shapes $geoWithin: { $geometry: { type: \"Point\", coordinates: [ 1, 1 ] }, $box: [ [ 0, 0 ], [ 1, 1 ] ] }",
        ),
        (
            Intersects,
            r#"{"$box": [[0, 0], [5, 5]]}"#,
            "$geoIntersect not supported with provided geometry: { $geoIntersects: { $box: [ [ 0, 0 ], [ 5, 5 ] ] } }",
        ),
        (Within, r#"{"$center": [[0, 0], -1]}"#, "Radius must be a non-negative number: -1"),
    ];
    for (op, spec, msg) in cases {
        assert_eq!(error(op, spec), (2, msg.to_string()), "{spec}");
    }
}

#[test]
fn a_geometry_partly_along_the_boundary_is_not_within() {
    // Measured on refnode 9.0.4, with and without a 2dsphere index: polygons and lines along part
    // of a side are not within the square; along a whole side, or equal to it, they are.
    let poly = |c: &str| format!(r#"{{"type": "Polygon", "coordinates": [{c}]}}"#);
    let line = |c: &str| format!(r#"{{"type": "LineString", "coordinates": {c}}}"#);
    let docs = [
        (1, poly("[[1, 1], [3, 1], [3, 3], [1, 3], [1, 1]]"), true),
        (2, poly("[[1, 0], [3, 0], [3, 3], [1, 3], [1, 0]]"), false),
        (3, poly("[[0, 1], [3, 1], [3, 3], [0, 3], [0, 1]]"), false),
        (4, poly("[[0, 0], [3, 0], [3, 3], [0, 3], [0, 0]]"), false),
        (5, poly("[[1, 1], [5, 1], [5, 3], [1, 3], [1, 1]]"), false),
        (6, poly("[[0, 0], [5, 0], [5, 5], [0, 5], [0, 0]]"), true),
        (7, line("[[0, 0], [5, 0]]"), true),
        (8, line("[[1, 0], [3, 0]]"), false),
        (9, line("[[0, 1], [0, 3]]"), false),
    ];
    let spec = wrap(&format!(r#"{{"$geometry": {SQUARE}}}"#));
    let spec = RawDoc::from_bytes(&spec).unwrap().get("v").unwrap();
    let shape = parse_query(GeoOperator::Within, spec).unwrap();
    for (id, p, want) in docs {
        let bytes = wrap(&p);
        assert_eq!(
            matches(&shape, RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap()),
            want,
            "{id}"
        );
    }
}
