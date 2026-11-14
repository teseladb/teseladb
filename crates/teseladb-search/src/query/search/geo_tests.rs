//! `geoWithin`, `geoShape` and `near` on `geo` fields as the reference search server 1.70.6 runs them (measured
//! 02-11-2026, `scratchpad/lane3j/m7.js`, whose documents and indexes are these).

use teseladb_bson::{Document, Value};

use super::*;
use crate::definition::Definition;

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

fn pos(x: f64, y: f64) -> Value {
    arr(vec![x.into(), y.into()])
}

fn point(x: f64, y: f64) -> Value {
    Document::new().with("type", "Point").with("coordinates", pos(x, y)).into()
}

fn ring(points: &[(f64, f64)]) -> Value {
    arr(points.iter().map(|&(x, y)| pos(x, y)).collect())
}

fn polygon(points: &[(f64, f64)]) -> Value {
    Document::new().with("type", "Polygon").with("coordinates", arr(vec![ring(points)])).into()
}

const SMALL: [(f64, f64); 5] =
    [(-4.0, 40.0), (-3.6, 40.0), (-3.6, 41.0), (-4.0, 41.0), (-4.0, 40.0)];
const BIG: [(f64, f64); 5] =
    [(-10.0, 35.0), (5.0, 35.0), (5.0, 45.0), (-10.0, 45.0), (-10.0, 35.0)];

fn index(shapes: bool) -> MemIndex {
    let mut g = Document::new().with("type", "geo");
    if shapes {
        g.push("indexShapes", true);
    }
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", false).with("fields", Document::new().with("g", g)),
    );
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    let line = Document::new()
        .with("type", "LineString")
        .with("coordinates", ring(&[(-5.0, 39.0), (3.0, 42.0)]));
    let multi = Document::new()
        .with("type", "MultiPoint")
        .with("coordinates", ring(&[(0.0, 0.0), (-3.5, 40.5)]));
    let docs = [
        Document::new().with("g", point(-3.70, 40.42)),
        Document::new().with("g", point(2.17, 41.39)),
        Document::new().with(
            "g",
            polygon(&[(-4.0, 40.0), (-3.0, 40.0), (-3.0, 41.0), (-4.0, 41.0), (-4.0, 40.0)]),
        ),
        Document::new().with("g", line),
        Document::new().with("g", multi),
        Document::new().with("g", pos(-3.70, 40.42)),
        Document::new().with("g", point(200.0, 0.0)),
    ];
    for (i, d) in docs.iter().enumerate() {
        idx.put(&[i as u8 + 1], d).expect("indexed");
    }
    idx
}

fn found(idx: &MemIndex, spec: &Document) -> Result<Vec<(u8, f64)>, String> {
    run(idx, spec)
        .map(|hits| hits.into_iter().map(|h| (h.key[0], f64::from(h.score))).collect())
        .map_err(|e| e.0)
}

fn ids(idx: &MemIndex, spec: &Document) -> Vec<u8> {
    let mut v: Vec<u8> = found(idx, spec).expect("runs").into_iter().map(|h| h.0).collect();
    v.sort_unstable();
    v
}

fn op(name: &str, args: Document) -> Document {
    Document::new().with(name, args)
}

fn within(region: &str, v: impl Into<Value>) -> Document {
    op("geoWithin", Document::new().with("path", "g").with(region, v))
}

fn geo_shape(relation: &str, g: Value) -> Document {
    op("geoShape", Document::new().with("path", "g").with("relation", relation).with("geometry", g))
}

#[test]
fn geo_within_finds_the_points_inside() {
    for shapes in [true, false] {
        let idx = index(shapes);
        let b = Document::new()
            .with("bottomLeft", point(-4.0, 40.0))
            .with("topRight", point(-3.0, 41.0));
        assert_eq!(ids(&idx, &within("box", b)), vec![1, 5]);
        let c = |r: f64| Document::new().with("center", point(-3.70, 40.42)).with("radius", r);
        assert_eq!(ids(&idx, &within("circle", c(1000.0))), vec![1]);
        assert_eq!(ids(&idx, &within("circle", c(600_000.0))), vec![1, 2, 5]);
        assert_eq!(ids(&idx, &within("geometry", polygon(&SMALL))), vec![1]);
        assert_eq!(ids(&idx, &within("geometry", polygon(&BIG))), vec![1, 2, 5]);
        let multi = Document::new()
            .with("type", "MultiPolygon")
            .with("coordinates", arr(vec![arr(vec![ring(&SMALL)])]));
        assert_eq!(ids(&idx, &within("geometry", multi)), vec![1]);
    }
    let idx = index(true);
    let b =
        Document::new().with("bottomLeft", point(-4.0, 40.0)).with("topRight", point(-3.0, 41.0));
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    let spec =
        op("geoWithin", Document::new().with("path", "g").with("box", b).with("score", boost));
    assert_eq!(found(&idx, &spec), Ok(vec![(1, 2.0), (5, 2.0)]));
    // Points left, right, below and above a box.
    for (x0, y0, x1, y1) in [
        (-3.0, 40.0, 3.0, 42.0),
        (-5.0, 40.0, -3.8, 42.0),
        (-5.0, 40.5, 3.0, 42.0),
        (-5.0, 35.0, 3.0, 40.45),
    ] {
        let b = Document::new().with("bottomLeft", point(x0, y0)).with("topRight", point(x1, y1));
        assert!(ids(&idx, &within("box", b)).len() <= 3);
    }
    let line = Document::new()
        .with("type", "LineString")
        .with("coordinates", ring(&[(0.0, 0.0), (1.0, 1.0)]));
    assert_eq!(
        found(&idx, &within("geometry", line)).unwrap_err(),
        "\"geoWithin.geometry\" Geometry must be a Polygon"
    );
    let other = op("geoWithin", Document::new().with("path", "h").with("geometry", polygon(&BIG)));
    assert_eq!(found(&idx, &other), Ok(vec![]));
}

#[test]
fn geo_shape_relates_the_shapes() {
    let idx = index(true);
    assert_eq!(ids(&idx, &geo_shape("intersects", polygon(&SMALL))), vec![1, 3]);
    assert_eq!(ids(&idx, &geo_shape("within", polygon(&SMALL))), vec![1]);
    assert_eq!(ids(&idx, &geo_shape("contains", point(-3.5, 40.5))), vec![3]);
    assert_eq!(ids(&idx, &geo_shape("disjoint", polygon(&SMALL))), vec![2, 4, 5]);
    assert_eq!(ids(&idx, &geo_shape("within", polygon(&BIG))), vec![1, 2, 3, 4]);
    let line = Document::new()
        .with("type", "LineString")
        .with("coordinates", ring(&[(-3.9, 39.0), (-3.9, 42.0)]));
    assert_eq!(ids(&idx, &geo_shape("intersects", line.into())), vec![3, 4]);
    let multiline = Document::new()
        .with("type", "MultiLineString")
        .with("coordinates", arr(vec![ring(&[(-3.9, 39.0), (-3.9, 42.0)])]));
    assert_eq!(ids(&idx, &geo_shape("intersects", multiline.into())), vec![3, 4]);
    // A polygon with a hole leaves out what the hole holds.
    let holed = Document::new().with("type", "Polygon").with(
        "coordinates",
        arr(vec![
            ring(&BIG),
            ring(&[(-3.8, 40.3), (-3.6, 40.3), (-3.6, 40.5), (-3.8, 40.5), (-3.8, 40.3)]),
        ]),
    );
    assert_eq!(ids(&idx, &geo_shape("within", holed.into())), vec![2, 4]);
}

#[test]
fn near_a_point_scores_by_meters() {
    let idx = index(true);
    let near = |x: f64, y: f64, pivot: f64| {
        found(
            &idx,
            &op(
                "near",
                Document::new().with("path", "g").with("origin", point(x, y)).with("pivot", pivot),
            ),
        )
    };
    assert_eq!(
        near(-3.70, 40.42, 1000.0),
        Ok(vec![(1, 1.0), (5, 0.04971024766564369), (2, 0.001976815750822425)])
    );
    assert_eq!(
        near(-3.70, 40.43, 1000.0),
        Ok(vec![(1, 0.47349563241004944), (5, 0.05095794051885605), (2, 0.0019778809510171413)])
    );
    assert_eq!(
        near(2.0, 41.0, 100_000.0),
        Ok(vec![(2, 0.6866281628608704), (5, 0.1765068620443344), (1, 0.17103908956050873)])
    );
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    let spec = op(
        "near",
        Document::new()
            .with("path", "g")
            .with("origin", point(0.0, 0.0))
            .with("pivot", 1)
            .with("score", boost),
    );
    assert_eq!(
        found(&idx, &spec),
        Ok(vec![(5, 2.0), (1, 4.434514835338632e-7), (2, 4.340704435890075e-7)])
    );
    let other = op(
        "near",
        Document::new().with("path", "t").with("origin", point(0.0, 0.0)).with("pivot", 1),
    );
    assert_eq!(found(&idx, &other), Ok(vec![]));
}

#[test]
fn geo_errors_are_the_reference_messages() {
    let idx = index(true);
    let err = |spec: Document| found(&idx, &spec).unwrap_err();
    let b = || {
        Document::new().with("bottomLeft", point(-4.0, 40.0)).with("topRight", point(-3.0, 41.0))
    };
    assert_eq!(
        err(op("geoWithin", Document::new().with("path", "g"))),
        "\"geoWithin\" one of [circle, box, geometry] must be present"
    );
    assert_eq!(
        err(op("geoWithin", Document::new().with("box", b()))),
        "\"geoWithin.path\" is required"
    );
    assert_eq!(
        err(op("geoWithin", Document::new().with("path", 1).with("box", b()))),
        "\"geoWithin.path\" must be a string"
    );
    assert_eq!(
        err(within("box", Document::new().with("bottomLeft", point(-4.0, 40.0)))),
        "\"geoWithin.box.topRight\" is required"
    );
    assert_eq!(
        err(within("circle", Document::new().with("center", point(0.0, 0.0)).with("radius", -1))),
        "\"geoWithin.circle.radius\" cannot be negative"
    );
    assert_eq!(
        err(within("circle", Document::new().with("center", point(0.0, 0.0)))),
        "\"geoWithin.circle.radius\" is required"
    );
    assert_eq!(
        err(within("circle", Document::new().with("center", point(0.0, 0.0)).with("radius", "a"))),
        "\"geoWithin.circle.radius\" must be a number"
    );
    assert_eq!(err(within("circle", 1)), "\"geoWithin.circle\" must be a document");
    assert_eq!(err(within("box", 1)), "\"geoWithin.box\" must be a document");
    assert_eq!(
        err(within("geometry", point(0.0, 0.0))),
        "\"geoWithin.geometry\" Geometry must be a Polygon"
    );
    assert_eq!(
        err(op(
            "geoWithin",
            Document::new().with("path", "g").with("box", b()).with("circle", Document::new())
        )),
        "\"geoWithin\" only one of [circle, box, geometry] may be present"
    );
    assert_eq!(
        err(op("geoWithin", Document::new().with("path", "g").with("box", b()).with("x", 1))),
        "\"geoWithin\" unrecognized field \"x\""
    );
    let shape_doc = |d: Document| op("geoShape", d);
    assert_eq!(
        err(shape_doc(Document::new().with("path", "g").with("geometry", polygon(&SMALL)))),
        "\"geoShape.relation\" is required"
    );
    assert_eq!(
        err(geo_shape("x", polygon(&SMALL))),
        "\"geoShape.relation\" must be one of [contains, disjoint, intersects, within]"
    );
    assert_eq!(
        err(shape_doc(Document::new().with("path", "g").with("relation", "within"))),
        "\"geoShape.geometry\" is required"
    );
    let on_t = shape_doc(
        Document::new()
            .with("path", "t")
            .with("relation", "within")
            .with("geometry", polygon(&SMALL)),
    );
    assert_eq!(
        err(on_t),
        "geoShape requires path 't' to be indexed as 'geo' with indexShapes=true"
    );
    let points_only = index(false);
    assert_eq!(
        found(&points_only, &geo_shape("within", polygon(&BIG))).unwrap_err(),
        "geoShape requires path 'g' to be indexed as 'geo' with indexShapes=true"
    );
    assert_eq!(
        err(geo_shape("within", polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]))),
        "\"geoShape.geometry.coordinates[0]\" at least 4 positions must be present (found 3)"
    );
    assert_eq!(
        err(op(
            "near",
            Document::new().with("path", "g").with("origin", polygon(&SMALL)).with("pivot", 1)
        )),
        "\"near.origin\" Expected type to be Point, got POLYGON"
    );
    // Shapes the query cannot read.
    let bad = |g: Value| err(geo_shape("within", g));
    assert_eq!(bad(1.into()), "\"geoShape.geometry\" must be a document");
    assert_eq!(bad(Document::new().into()), "\"geoShape.geometry.type\" is required");
    assert_eq!(
        bad(Document::new().with("type", "Point").into()),
        "\"geoShape.geometry.coordinates\" is required"
    );
    assert_eq!(
        bad(Document::new().with("type", "Circle").with("coordinates", 1).into()),
        "\"geoShape.geometry.type\" is not a supported geometry"
    );
    let p = |c: Value| bad(Document::new().with("type", "Point").with("coordinates", c).into());
    assert_eq!(p(pos(0.0, 91.0)), "\"geoShape.geometry.coordinates\" position is out of bounds");
    assert_eq!(
        p(arr(vec!["a".into(), 1.into()])),
        "\"geoShape.geometry.coordinates\" must be an array of numbers"
    );
    assert_eq!(
        p(arr(vec![1.into()])),
        "\"geoShape.geometry.coordinates\" must be an array of at least two numbers"
    );
    let m = |kind: &str, c: Value| {
        bad(Document::new().with("type", kind).with("coordinates", c).into())
    };
    assert_eq!(m("MultiPoint", 1.into()), "\"geoShape.geometry.coordinates\" must be an array");
    assert_eq!(
        m("MultiLineString", 1.into()),
        "\"geoShape.geometry.coordinates\" must be an array"
    );
    assert_eq!(m("MultiPolygon", 1.into()), "\"geoShape.geometry.coordinates\" must be an array");
    assert_eq!(m("Polygon", 1.into()), "\"geoShape.geometry.coordinates\" must be an array");
    assert_eq!(
        m("Polygon", arr(vec![ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)])])),
        "\"geoShape.geometry.coordinates[0]\" first and last positions must be the same"
    );
    assert_eq!(
        err(op(
            "near",
            Document::new()
                .with("path", "g")
                .with("origin", Document::new().with("type", "Point"))
                .with("pivot", 1)
        )),
        "\"near.origin.coordinates\" is required"
    );
    assert_eq!(
        err(op(
            "near",
            Document::new()
                .with("path", "g")
                .with("origin", Document::new().with("type", 1).with("coordinates", 1))
                .with("pivot", 1)
        )),
        "\"near.origin.type\" is required"
    );
}
