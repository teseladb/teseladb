//! `$geoNear` (010 FR-004): its arguments, the index it picks among the collection's 2d and
//! 2dsphere indexes, and the distances it gives, with the codes of a reference router 9.0.4 (the same
//! cases run through the protocol in the tests of `teseladb-wire`).

use teseladb_bson::{Document, RawValue, Value, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::geo_near::GeoIndex;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

struct GeoEnv {
    indexes: Vec<GeoIndex>,
}

impl Env for GeoEnv {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
        Ok(Vec::new())
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn geo_indexes(&self) -> Result<(String, Vec<GeoIndex>), MqlError> {
        Ok(("g.c".to_owned(), self.indexes.clone()))
    }
}

fn flat(field: &str) -> GeoIndex {
    GeoIndex { field: field.to_owned(), sphere: false }
}

fn round(field: &str) -> GeoIndex {
    GeoIndex { field: field.to_owned(), sphere: true }
}

fn parse(spec: &str) -> Result<Pipeline, i32> {
    let holder = extjson::parse(&format!(r#"{{"p":[{{"$geoNear":{spec}}}]}}"#)).expect("JSON");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse(&stages, &Ctx::default()).map_err(|e| e.code.code)
}

fn docs() -> Vec<Document> {
    [
        r#"{"_id":1,"p":[1,1],"s":{"type":"Point","coordinates":[1,1]},"k":"a"}"#,
        r#"{"_id":2,"p":[3,4],"s":{"type":"Point","coordinates":[3,4]},"k":"b"}"#,
        r#"{"_id":3,"p":[0.5,0],"s":{"type":"Point","coordinates":[0.5,0]},"k":"a"}"#,
        r#"{"_id":4,"p":"none","s":{"type":"Nowhere"}}"#,
        r#"{"_id":5}"#,
    ]
    .iter()
    .map(|t| extjson::parse(t).expect("a document"))
    .collect()
}

fn run(spec: &str, indexes: Vec<GeoIndex>) -> Result<Vec<Document>, i32> {
    let p = parse(spec)?;
    p.run(docs(), &GeoEnv { indexes }).map_err(|e| e.code.code)
}

fn ids(out: &[Document]) -> Vec<i32> {
    out.iter()
        .map(|d| match d.get("_id") {
            Some(Value::Int32(n)) => *n,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn arguments_are_checked() {
    for (spec, code) in [
        ("1", 10065),
        (r#"{"distanceField":"d"}"#, 5860400),
        (r#"{"near":"x"}"#, 5860401),
        (r#"{"near":[0,0],"distanceField":1}"#, 16606),
        (r#"{"near":[0,0],"includeLocs":1}"#, 16607),
        (r#"{"near":[0,0],"maxDistance":"x"}"#, 14),
        (r#"{"near":[0,0],"minDistance":-1}"#, 2),
        (r#"{"near":[0,0],"distanceMultiplier":"x"}"#, 14),
        (r#"{"near":[0,0],"distanceMultiplier":-1}"#, 2),
        (r#"{"near":[0,0],"query":1}"#, 14),
        (r#"{"near":[0,0],"key":""}"#, 2),
        (r#"{"near":[0,0],"key":1}"#, 14),
        (r#"{"near":[0,0],"other":1}"#, 2),
        (r#"{"near":{"type":"Line","coordinates":[0,0]}}"#, 2),
        (r#"{"near":{"type":"Point","coordinates":[200,0]}}"#, 2),
        (r#"{"near":{"type":"Point","coordinates":"x"}}"#, 2),
        (r#"{"near":["x"]}"#, 2),
        (r#"{"near":{"x":"y"}}"#, 2),
    ] {
        assert_eq!(parse(spec).map(|_| ()), Err(code), "{spec}");
    }
    assert!(parse(r#"{"near":{"x":1,"y":2},"spherical":true}"#).is_ok());
}

#[test]
fn a_flat_index_orders_by_plane_distance() {
    let out = run(
        r#"{"near":[0,0],"distanceField":"d.v","includeLocs":"loc","distanceMultiplier":2,"maxDistance":4}"#,
        vec![flat("p")],
    )
    .expect("runs");
    assert_eq!(ids(&out), [3, 1]);
    let d = match out[0].get("d") {
        Some(Value::Document(d)) => d.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(d.get("v"), Some(&Value::Double(1.0)));
    assert!(out[0].get("loc").is_some());
    let out =
        run(r#"{"near":[0,0],"minDistance":1,"query":{"k":"a"}}"#, vec![flat("p")]).expect("runs");
    assert_eq!(ids(&out), [1]);
    let sphere = run(r#"{"near":[0,0],"spherical":true}"#, vec![flat("p")]).expect("runs");
    assert_eq!(ids(&sphere), [3, 1, 2]);
    assert_eq!(run(r#"{"near":[500,0]}"#, vec![flat("p")]).map(|_| ()), Err(16433));
}

#[test]
fn a_sphere_index_measures_on_the_earth() {
    let geo = run(
        r#"{"near":{"type":"Point","coordinates":[0,0]},"distanceField":"d"}"#,
        vec![round("s")],
    )
    .expect("runs");
    assert_eq!(ids(&geo), [3, 1, 2]);
    let Some(Value::Double(m)) = geo[0].get("d") else { panic!("{:?}", geo[0]) };
    assert!((m - 55_659.7).abs() < 1.0, "{m}");
    let legacy = run(r#"{"near":[0,0],"spherical":true,"distanceField":"d"}"#, vec![round("s")])
        .expect("runs");
    let Some(Value::Double(r)) = legacy[0].get("d") else { panic!("{:?}", legacy[0]) };
    assert!((r - 0.008_726_6).abs() < 1e-6, "{r}");
}

#[test]
fn the_index_must_fit_the_query() {
    assert_eq!(run(r#"{"near":[0,0]}"#, vec![]).map(|_| ()), Err(27));
    assert_eq!(run(r#"{"near":[0,0]}"#, vec![flat("p"), flat("q")]).map(|_| ()), Err(2));
    assert!(run(r#"{"near":[0,0],"key":"p"}"#, vec![flat("p"), flat("q")]).is_ok());
    assert!(run(r#"{"near":[0,0]}"#, vec![flat("p"), round("s")]).is_ok());
    assert_eq!(run(r#"{"near":[0,0],"key":"z"}"#, vec![flat("p")]).map(|_| ()), Err(291));
    assert_eq!(run(r#"{"near":[0,0]}"#, vec![round("s")]).map(|_| ()), Err(291));
    let geo = r#"{"near":{"type":"Point","coordinates":[0,0]},"maxDistance":5}"#;
    assert_eq!(run(geo, vec![flat("p")]).map(|_| ()), Err(291));
    let p = parse(r#"{"near":[0,0]}"#).expect("valid");
    assert_eq!(p.check_plan(&GeoEnv { indexes: vec![] }).map_err(|e| e.code.code), Err(27));
}
