//! Edge cases of the sphere geometry and of GeoJSON reading (009 T019, FR-011, FR-012) that the
//! measured vectors do not reach.

use std::f64::consts::PI;

use teseladb_bson::{RawDoc, extjson};
use teseladb_index::geo::geojson::{GeoError, Geometry, parse, type_name};
use teseladb_index::geo::region::{Container, intersects, within};
use teseladb_index::geo::sphere::{
    Cap, Crossing, Loop, Point, Polygon, crossing, distance_to_edge,
};
use teseladb_index::shell::shell_value;

fn geo(text: &str) -> Result<Geometry, GeoError> {
    let bytes = extjson::parse(&format!("{{\"v\": {text}}}")).unwrap().to_bytes().unwrap();
    parse(RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap())
}

fn reason(text: &str) -> String {
    geo(text).unwrap_err().0
}

fn d(lng: f64, lat: f64) -> Point {
    Point::from_degrees(lng, lat)
}

#[test]
fn points_and_vectors() {
    assert_eq!(Point([0.0; 3]).normalize(), Point([0.0; 3]));
    let (lng, lat) = d(-73.5, 40.25).to_degrees();
    assert!((lng + 73.5).abs() < 1e-12 && (lat - 40.25).abs() < 1e-12);
    assert!((d(0.0, 0.0).angle(d(90.0, 0.0)) - PI / 2.0).abs() < 1e-15);
    assert_eq!(
        distance_to_edge(d(1.0, 1.0), d(0.0, 0.0), d(0.0, 0.0)),
        d(1.0, 1.0).angle(d(0.0, 0.0))
    );
    assert!(
        distance_to_edge(d(5.0, 1.0), d(0.0, 0.0), d(2.0, 0.0))
            > distance_to_edge(d(1.0, 1.0), d(0.0, 0.0), d(2.0, 0.0))
    );
}

#[test]
fn edges_cross_touch_or_miss() {
    let (a, b, c, e) = (d(0.0, 0.0), d(2.0, 2.0), d(0.0, 2.0), d(2.0, 0.0));
    assert_eq!(crossing(a, b, c, e), Crossing::Cross);
    assert_eq!(crossing(a, c, e, b), Crossing::None);
    assert_eq!(crossing(a, b, b, c), Crossing::Touch);
    // Collinear but apart, and an antipodal pair on the same great circle.
    assert_eq!(crossing(d(0.0, 0.0), d(1.0, 0.0), d(2.0, 0.0), d(3.0, 0.0)), Crossing::None);
    assert_eq!(crossing(d(0.0, 0.0), d(1.0, 0.0), d(0.5, 0.0), d(3.0, 0.0)), Crossing::Touch);
    assert_eq!(crossing(d(0.0, 1.0), d(0.0, 2.0), d(180.0, -1.0), d(180.0, -2.0)), Crossing::None);
}

#[test]
fn loops_and_polygons() {
    assert_eq!(Loop::left_area(&[]), 0.0);
    let ccw = vec![d(0.0, 0.0), d(1.0, 0.0), d(1.0, 1.0)];
    let mut cw = ccw.clone();
    cw.reverse();
    let small = Loop::smaller(cw.clone());
    assert!(small.area() < 1e-3);
    assert!(Loop::new(cw).area() > 4.0 * PI - 1e-3, "the left side of a clockwise loop is large");
    assert!(small.contains(d(0.7, 0.3)) && !small.contains(d(5.0, 5.0)));
    assert!(!Polygon { loops: vec![] }.contains(d(0.0, 0.0)));
    let doubled = Loop::new(vec![d(0.0, 0.0), d(2.0, 0.0), d(1.0, 0.0), d(0.0, 1.0)]);
    assert!(doubled.self_crossing().is_some(), "an edge that doubles back over the previous one");
    let touching = Loop::new(vec![d(0.0, 0.0), d(2.0, 0.0), d(2.0, 2.0), d(1.0, 0.0), d(0.0, 2.0)]);
    assert!(touching.self_crossing().is_some(), "a vertex on a non-adjacent edge");
    assert!(Loop::new(vec![d(0.0, 0.0), d(1.0, 0.0)]).contains(d(0.0, 0.0)));
}

/// Reasons measured on refnode 9.0.4 for values outside `tests/data/geo-reading.json`.
#[test]
fn reading_rejections_measured_one_by_one() {
    assert_eq!(
        reason("{\"type\": \"LineString\"}"),
        "GeoJSON coordinates must be an array of coordinates, instead got type missing"
    );
    assert_eq!(
        reason("{\"type\": \"Polygon\", \"coordinates\": [[[0, 0]]]}"),
        "Loop must have at least 3 different vertices, 0 unique vertices were provided: [ [ 0, 0 ] ]"
    );
    assert_eq!(
        reason("{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [1, 1]]]}"),
        "Loop is not closed, first vertex does not equal last vertex: [ [ 0, 0 ], [ 1, 1 ] ]"
    );
    assert_eq!(
        reason("{\"type\": \"Polygon\", \"coordinates\": [5]}"),
        "GeoJSON coordinates must be an array of coordinates, instead got type int"
    );
    assert_eq!(
        reason("{\"type\": \"Point\", \"coordinates\": [0, -91]}"),
        "Longitude/latitude is out of bounds, lng: 0 lat: -91"
    );
    assert_eq!(
        reason("{\"type\": \"GeometryCollection\", \"geometries\": [1]}"),
        "Element 0 of \"geometries\" must be an object, instead got type int: 1"
    );
    assert_eq!(
        reason("[{\"$numberDecimal\": \"500\"}, 0]"),
        "can't project geometry from FLAT CRS into spherical CRS"
    );
    assert!(geo("{\"type\": \"Point\", \"coordinates\": [1, 2], \"crs\": {\"type\": \"name\", \"properties\": {\"name\": \"EPSG:4326\"}}}").is_ok());
    assert_eq!(
        reason("{\"type\": \"Point\", \"coordinates\": [1, 2], \"crs\": {\"type\": \"name\"}}"),
        "CRS must have field \"properties\" which is an object, instead got type missing"
    );
    assert!(geo("{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [1, 0], [1, 1], [0, 0]]], \"crs\": {\"type\": \"name\", \"properties\": {\"name\": \"urn:x-refdb:crs:strictwinding:EPSG:4326\"}}}").is_ok());
    assert_eq!(type_name(None), "missing");
    assert!(geo("[{\"$numberLong\": \"10\"}, {\"$numberDecimal\": \"5\"}]").is_ok());
}

#[test]
fn holes_that_touch_or_nest() {
    let ok = "{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]], [[1, 1], [2, 1], [2, 2], [1, 1]], [[5, 5], [6, 5], [6, 6], [5, 5]]]}";
    assert!(geo(ok).is_ok());
    let nested = "{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]], [[1, 1], [5, 1], [5, 5], [1, 5], [1, 1]], [[2, 2], [3, 2], [3, 3], [2, 2]]]}";
    assert!(reason(nested).starts_with("Polygon isn't valid: Loop 1 crosses loop 2"));
    let around = "{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]], [[2, 2], [3, 2], [3, 3], [2, 2]], [[1, 1], [5, 1], [5, 5], [1, 5], [1, 1]]]}";
    assert!(reason(around).starts_with("Polygon isn't valid: Loop 1 crosses loop 2"));
}

#[test]
fn relations_between_shapes() {
    let square = geo("{\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [4, 0], [4, 4], [0, 4], [0, 0]], [[1, 1], [1, 2], [2, 2], [2, 1], [1, 1]]]}").unwrap();
    let Geometry::Polygon(sq) = square.clone() else { panic!() };
    let container = Container::Polygons(vec![sq]);
    let inner = geo(
        "{\"type\": \"Polygon\", \"coordinates\": [[[2.5, 2.5], [3, 2.5], [3, 3], [2.5, 2.5]]]}",
    )
    .unwrap();
    let around_hole = geo("{\"type\": \"Polygon\", \"coordinates\": [[[0.5, 0.5], [3, 0.5], [3, 3], [0.5, 3], [0.5, 0.5]]]}").unwrap();
    let line = geo("{\"type\": \"LineString\", \"coordinates\": [[0.5, 3], [3.5, 3]]}").unwrap();
    let out_line = geo("{\"type\": \"LineString\", \"coordinates\": [[0.5, 3], [5, 3]]}").unwrap();
    assert!(within(&inner, &container));
    assert!(!within(&around_hole, &container), "the hole is inside it");
    assert!(within(&line, &container) && !within(&out_line, &container));
    assert!(intersects(&out_line, &square) && intersects(&square, &inner));
    assert!(intersects(&line, &out_line));
    let far = geo("{\"type\": \"MultiPoint\", \"coordinates\": [[50, 50], [60, 60]]}").unwrap();
    assert!(!intersects(&far, &square) && !intersects(&far, &line));
    assert!(intersects(&geo("[3.5, 3]").unwrap(), &line));
    let cap = Container::Cap(Cap { center: d(2.0, 2.0), radius: 0.1 });
    assert!(within(&line, &cap) && within(&inner, &cap) && within(&geo("[2, 2]").unwrap(), &cap));
    let huge = Container::Cap(Cap { center: d(2.0, 2.0), radius: 2.0 });
    assert!(
        !within(&line, &huge) && !within(&inner, &huge),
        "a cap past a hemisphere is not convex"
    );
    let collection = geo("{\"type\": \"GeometryCollection\", \"geometries\": [{\"type\": \"Point\", \"coordinates\": [2.5, 2.6]}, {\"type\": \"MultiLineString\", \"coordinates\": [[[3, 3], [3.5, 3.5]]]}]}").unwrap();
    assert!(within(&collection, &container));
    assert!(intersects(&geo("{\"type\": \"MultiPolygon\", \"coordinates\": [[[[3, 3], [3.5, 3], [3.5, 3.5], [3, 3]]]]}").unwrap(), &collection));
}

#[test]
fn shell_values() {
    let text = "{\"v\": {\"b\": {\"$binary\": {\"base64\": \"q80=\", \"subType\": \"00\"}}, \"u\": {\"$binary\": {\"base64\": \"ABEiM0RVZneImaq7zN3u/w==\", \"subType\": \"04\"}}, \"s\": {\"$symbol\": \"x\"}, \"c\": {\"$code\": \"f()\"}, \"w\": {\"$code\": \"g\", \"$scope\": {\"a\": 1}}, \"p\": {\"$dbPointer\": {\"$ref\": \"db.c\", \"$id\": {\"$oid\": \"5f0000000000000000000001\"}}}, \"n\": {\"$undefined\": true}, \"r\": {\"$regularExpression\": {\"pattern\": \"a\", \"options\": \"i\"}}, \"t\": {\"$timestamp\": {\"t\": 1, \"i\": 2}}, \"l\": {\"$numberLong\": \"5\"}, \"m\": {\"$minKey\": 1}, \"x\": {\"$maxKey\": 1}, \"d\": {\"$date\": {\"$numberLong\": \"-1\"}}, \"e\": {\"$numberDecimal\": \"1.50\"}, \"f\": false}}";
    let bytes = extjson::parse(text).unwrap().to_bytes().unwrap();
    let v = RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap();
    assert_eq!(
        shell_value(v),
        "{ b: BinData(0, ABCD), u: UUID(\"00112233-4455-6677-8899-aabbccddeeff\"), s: \"x\", c: f(), w: CodeWScope( g, { a: 1 }), p: DBRef('db.c', 5f0000000000000000000001), n: undefined, r: /a/i, t: Timestamp(1, 2), l: 5, m: MinKey, x: MaxKey, d: new Date(-1), e: 1.50, f: false }"
    );
}
