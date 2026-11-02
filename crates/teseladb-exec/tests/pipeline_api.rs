//! The parts of a parsed pipeline the node asks about before it runs it (010 FR-004): the
//! slot-based prefix `explain` shows, the system stage it answers itself, `aggregate: 1`'s
//! first stage, and the defaults of an environment.

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

struct Plain;

impl Env for Plain {
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
        0.25
    }
}

fn parse(text: &str, on_collection: bool) -> Result<Pipeline, i32> {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("JSON");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse_command(&stages, &Ctx::default(), on_collection).map_err(|e| e.code.code)
}

#[test]
fn the_slot_based_prefix_stops_at_the_first_classic_stage() {
    let p = parse(
        r#"[{"$match":{"a":1}},{"$group":{"_id":"$a"}},{"$redact":"$$KEEP"},{"$limit":1}]"#,
        true,
    )
    .expect("valid");
    assert_eq!(p.sbe_prefix(), 2);
    assert!(!p.runs_in_sbe());
    let docs = parse(r#"[{"$documents":[{"a":1}]},{"$match":{}}]"#, false).expect("valid");
    assert_eq!(docs.sbe_prefix(), 0);
    assert!(!docs.reads_index_key());
    assert_eq!(docs.system_stage(), None);
    let stats = parse(r#"[{"$collStats":{"count":{}}}]"#, true).expect("valid");
    assert_eq!(stats.system_stage(), Some("$collStats"));
}

#[test]
fn aggregate_one_needs_a_stage_that_reads_no_collection() {
    assert_eq!(parse("[]", false).map(|_| ()), Err(73));
    assert_eq!(parse(r#"[{"$listSessions":{}}]"#, false).map(|_| ()), Err(73));
    assert_eq!(parse(r#"[{"$match":{}}]"#, false).map(|_| ()), Err(73));
    assert!(parse(r#"[{"$currentOp":{}}]"#, false).is_ok());
    assert!(parse("[]", true).is_ok());
}

#[test]
fn an_empty_pipeline_and_an_inhibitor_pass_documents_on() {
    let d: Document = extjson::parse(r#"{"a":1}"#).expect("a document");
    let empty = parse("[]", true).expect("valid");
    assert_eq!(empty.run(vec![d.clone()], &Plain).expect("runs"), vec![d.clone()]);
    let p = parse(r#"[{"$_internalInhibitOptimization":{}},{"$sample":{"size":1}}]"#, true)
        .expect("valid");
    assert_eq!(p.run(vec![d.clone()], &Plain).expect("runs"), vec![d]);
}

#[test]
fn an_environment_has_no_search_and_no_geo_index_by_default() {
    assert!(Plain.check_search("$search", &Document::new()).is_ok());
    assert_eq!(Plain.geo_indexes().expect("none"), (String::new(), Vec::new()));
}

/// An environment whose every other collection holds `docs`.
struct Foreign(Vec<Document>);

impl Env for Foreign {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
        Ok(self.0.clone())
    }

    fn random(&self) -> f64 {
        0.25
    }
}

#[test]
fn a_lookup_reads_a_numbered_place_of_a_foreign_array() {
    let foreign = Foreign(vec![
        extjson::parse(r#"{"_id":1,"arr":[5,7]}"#).expect("doc"),
        extjson::parse(r#"{"_id":2,"arr":[7,9]}"#).expect("doc"),
        extjson::parse(r#"{"_id":3,"arr":5}"#).expect("doc"),
    ]);
    let local: Document = extjson::parse(r#"{"k":7}"#).expect("doc");
    let joined = |field: &str| -> Vec<i32> {
        let text = format!(
            r#"[{{"$lookup":{{"from":"o","localField":"k","foreignField":"{field}","as":"j"}}}}]"#
        );
        let out =
            parse(&text, true).expect("valid").run(vec![local.clone()], &foreign).expect("runs");
        let Some(teseladb_bson::Value::Array(j)) = out[0].get("j") else { panic!("{out:?}") };
        j.iter()
            .map(|d| match d {
                teseladb_bson::Value::Document(d) => match d.get("_id") {
                    Some(teseladb_bson::Value::Int32(n)) => *n,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(joined("arr.1"), [1]);
    assert_eq!(joined("arr.0"), [2]);
    assert_eq!(joined("arr.5"), Vec::<i32>::new());
    assert_eq!(joined("arr.x"), Vec::<i32>::new());
}

#[test]
fn a_facet_refuses_stages_that_are_not_documents() {
    assert!(parse(r#"[{"$facet":{"a":[5]}}]"#, true).is_err());
    assert!(parse(r#"[{"$facet":{"a":[{}]}}]"#, true).is_err());
}
