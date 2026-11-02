//! The arguments of the stages that read the node's own state (`$collStats`, `$currentOp`,
//! `$indexStats`, `$listSessions`…; 010 FR-004, FR-016, FR-018), with the codes of a `refrouter`
//! 9.0.4 (the same cases run through the protocol in the tests of `teseladb-wire`).

use teseladb_bson::{RawValue, Value, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::{Pipeline, scale_of};

fn parse(stage: &str, arg: &str) -> Result<(), i32> {
    let holder = extjson::parse(&format!(r#"{{"p":[{{"{stage}":{arg}}}]}}"#)).expect("JSON");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse(&stages, &Ctx::default()).map(|_| ()).map_err(|e| e.code.code)
}

fn check(stage: &str, cases: &[(&str, Result<(), i32>)]) {
    for (arg, want) in cases {
        assert_eq!(parse(stage, arg), *want, "{stage}: {arg}");
    }
}

#[test]
fn coll_stats_sections_and_their_options() {
    check(
        "$collStats",
        &[
            (r#"{"latencyStats":{"histograms":true},"count":{},"queryExecStats":{}}"#, Ok(())),
            (r#"{"storageStats":{"scale":1024,"verbose":true,"other":1}}"#, Ok(())),
            (r#"{"storageStats":{"scale":{"$numberDecimal":"2.5"}}}"#, Ok(())),
            ("1", Err(5_447_000)),
            (r#"{"x":{}}"#, Err(40415)),
            (r#"{"count":1}"#, Err(14)),
            (r#"{"count":{"a":1}}"#, Err(31170)),
            (r#"{"latencyStats":{"histograms":1}}"#, Err(14)),
            (r#"{"storageStats":{"waitForLock":"x"}}"#, Err(14)),
            (r#"{"storageStats":{"scale":"x"}}"#, Err(14)),
            (r#"{"storageStats":{"scale":0}}"#, Err(2)),
        ],
    );
    assert_eq!(scale_of(&Value::Double(3.7)), Some(3));
    assert_eq!(scale_of(&Value::Int64(5)), Some(5));
    assert_eq!(scale_of(&Value::from("x")), None);
}

#[test]
fn search_index_listing_takes_an_id_or_a_name() {
    check(
        "$listSearchIndexes",
        &[
            (r#"{"name":"a"}"#, Ok(())),
            (r#"{"id":"a"}"#, Ok(())),
            ("1", Err(9)),
            (r#"{"x":"a"}"#, Err(40415)),
            (r#"{"name":1}"#, Err(14)),
            (r#"{"name":"a","id":"b"}"#, Err(72)),
        ],
    );
}

#[test]
fn current_op_takes_boolean_flags() {
    check(
        "$currentOp",
        &[
            (r#"{"allUsers":true,"idleConnections":false,"localOps":true}"#, Ok(())),
            ("1", Err(14)),
            (r#"{"x":true}"#, Err(40415)),
            (r#"{"allUsers":1}"#, Err(14)),
        ],
    );
}

#[test]
fn sessions_take_all_users_or_a_list() {
    for stage in ["$listSessions", "$listLocalSessions"] {
        check(
            stage,
            &[
                (r#"{"allUsers":true}"#, Ok(())),
                (r#"{"users":[{"user":"u","db":"d"}]}"#, Ok(())),
                ("1", Err(14)),
                (r#"{"allUsers":1}"#, Err(14)),
                (r#"{"users":1}"#, Err(14)),
                (r#"{"users":[1]}"#, Err(14)),
                (r#"{"users":[{"user":1,"db":"d"}]}"#, Err(14)),
                (r#"{"users":[{"user":"u","db":"d","x":1}]}"#, Err(40415)),
                (r#"{"users":[{"user":"u"}]}"#, Err(40414)),
                (r#"{"x":1}"#, Err(40415)),
                (r#"{"allUsers":true,"users":[{"user":"u","db":"d"}]}"#, Err(12)),
            ],
        );
    }
}

#[test]
fn the_other_stages_take_their_objects() {
    check("$indexStats", &[("{}", Ok(())), (r#"{"a":1}"#, Err(28803)), ("1", Err(28803))]);
    check(
        "$planCacheStats",
        &[(r#"{"allHosts":true}"#, Ok(())), ("1", Err(9)), (r#"{"x":1}"#, Err(9))],
    );
    check(
        "$listClusterCatalog",
        &[(r#"{"shards":true}"#, Ok(())), ("1", Err(9_621_302)), (r#"{"x":1}"#, Err(40415))],
    );
    check("$listExtensions", &[("{}", Ok(())), (r#"{"a":1}"#, Err(10_983_502))]);
    check("$listCatalog", &[("{}", Ok(())), ("1", Err(6_200_600))]);
    check("$querySettings", &[("{}", Ok(())), ("1", Err(14))]);
    check("$queryStats", &[("{}", Ok(()))]);
    check("$listMqlEntities", &[("{}", Err(224))]);
}
