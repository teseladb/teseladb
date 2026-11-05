//! Spherical predicates (009 T019, FR-011, FR-012): `$geoWithin` and `$geoIntersects` over the
//! valid geometries of `geo-vectors.json` select the documents a real RefDb server selected
//! (`tests/data/geo-predicates.json`, same with and without a `2dsphere` index).

use teseladb_index::geo::geojson::{Geometry, number, parse, parse_indexed};
use teseladb_index::geo::region::{Container, intersects, within};
use teseladb_index::geo::sphere::{Cap, Point};
use teseladb_bson::{RawDoc, RawValue, Value, extjson};

const VECTORS: &str = include_str!("data/geo-vectors.json");
const PREDICATES: &str = include_str!("data/geo-predicates.json");

fn array(v: Option<&Value>) -> &Vec<Value> {
    match v {
        Some(Value::Array(a)) => a,
        other => panic!("not an array: {other:?}"),
    }
}

fn string(v: Option<&Value>) -> &str {
    match v {
        Some(Value::String(s)) => s,
        other => panic!("not a string: {other:?}"),
    }
}

/// The geometries the server stored, by `_id` (their position in the vectors).
fn documents() -> Vec<(i32, Geometry)> {
    let file = extjson::parse(VECTORS).unwrap();
    let mut out = Vec::new();
    for (id, v) in array(file.get("vectors")).iter().enumerate() {
        let Value::Document(v) = v else { panic!() };
        if v.get("err").is_some() {
            continue;
        }
        let bytes = extjson::parse(string(v.get("loc"))).unwrap().to_bytes().unwrap();
        let g = parse_indexed(RawDoc::from_bytes(&bytes).unwrap().get("v").unwrap()).unwrap();
        out.push((id as i32, g));
    }
    out
}

/// A query `{loc: {<op>: <arg>}}` as a test on geometries.
fn predicate(query: RawDoc<'_>) -> Box<dyn Fn(&Geometry) -> bool> {
    let Some(RawValue::Document(op)) = query.get("loc") else { panic!("no loc") };
    let (name, arg) = op.iter().next().unwrap();
    let RawValue::Document(arg) = arg else { panic!("argument") };
    if let Some(RawValue::Array(c)) = arg.get("$centerSphere") {
        let mut it = c.iter();
        let Some(RawValue::Array(center)) = it.next() else { panic!("center") };
        let mut xy = center.iter().map(|v| number(v).unwrap());
        let center = Point::from_degrees(xy.next().unwrap(), xy.next().unwrap());
        let radius = number(it.next().unwrap()).unwrap();
        let cap = Container::Cap(Cap { center, radius });
        return Box::new(move |g| within(g, &cap));
    }
    let shape = parse(arg.get("$geometry").unwrap()).unwrap();
    match name {
        "$geoIntersects" => Box::new(move |g| intersects(g, &shape)),
        _ => {
            let container = match shape {
                Geometry::Polygon(p) => Container::Polygons(vec![p]),
                Geometry::MultiPolygon(ps) => Container::Polygons(ps),
                other => panic!("container {other:?}"),
            };
            Box::new(move |g| within(g, &container))
        }
    }
}

/// Documents that only touch the query shape at a point of its boundary, where RefDb's answer
/// depends on rounding and TeselaDB follows RefDb's definition (closed shapes: a shared point
/// intersects, a point on the boundary is within; divergence proposed with 009 T019). Each entry:
/// position of the query in the file, document, and TeselaDB's answer.
const BOUNDARY_CONTACTS: [(usize, i32, bool); 6] = [
    // Point (−90, 0) on the edge at longitude −90 of the box from −180 to −90, and point
    // (180, 0), the same place as (−180, 0), on its edge at −180 (RefDb keeps (−180, 0)).
    (4, 51, true),
    (5, 51, true),
    (4, 43, true),
    (5, 43, true),
    // The line from (−180, 0) to (0, 0) along the equator starts at (180, 0) and passes
    // through (−90, 0). It ends at (0, 0), a vertex of polygon 112, which it does not meet:
    // RefDb clips the line to the polygon and nothing of positive length is left (measured,
    // and TeselaDB clips the same way).
    (23, 43, true),
    (23, 51, true),
];

#[test]
fn every_measured_query_selects_what_refdb_selected() {
    let docs = documents();
    let file = extjson::parse(PREDICATES).unwrap();
    let mut wrong = Vec::new();
    for (n, q) in array(file.get("queries")).iter().enumerate() {
        let Value::Document(q) = q else { panic!() };
        let text = string(q.get("query"));
        let bytes = extjson::parse(text).unwrap().to_bytes().unwrap();
        let test = predicate(RawDoc::from_bytes(&bytes).unwrap());
        let mut want: Vec<i32> = array(q.get("ids"))
            .iter()
            .map(|v| match v {
                Value::Int32(n) => *n,
                Value::Double(d) => *d as i32,
                other => panic!("id {other:?}"),
            })
            .collect();
        for &(query, id, answer) in &BOUNDARY_CONTACTS {
            if query == n {
                want.retain(|&x| x != id);
                if answer {
                    want.push(id);
                }
            }
        }
        want.sort_unstable();
        let got: Vec<i32> = docs.iter().filter(|(_, g)| test(g)).map(|(id, _)| *id).collect();
        if got != want {
            wrong.push(format!("{text}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{} queries differ:\n{}", wrong.len(), wrong.join("\n"));
}
