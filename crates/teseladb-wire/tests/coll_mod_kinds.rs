//! `collMod` of views, time series and the expiry of clustered collections (015 FR-001, FR-006,
//! FR-009; audit T020), with the replies and the options `listCollections` shows that a `refnode`
//! 9.0.4 gave on 02-11-2026.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("m", json(cmd))
}

fn options(m: &mut Mem, name: &str) -> Value {
    let filter = doc(&[("name", Value::from(name))]);
    let r = m.run(
        "m",
        doc(&[("listCollections", Value::Int32(1)), ("filter", Value::Document(filter))]),
    );
    field(&r, "cursor.firstBatch.0.options").clone()
}

/// Runs each `(command, outcome)`: `Ok` with the options of collection `name` after it, `Err`
/// with the code and message of its refusal.
fn check(m: &mut Mem, name: &str, cases: &[(&str, Result<&str, (i32, &str)>)]) {
    for (cmd, want) in cases {
        let r = run(m, cmd);
        match want {
            Ok(o) => {
                assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd}: {r:?}");
                assert_eq!(options(m, name), Value::Document(json(o)), "{cmd}");
            }
            Err((code, msg)) => {
                assert_eq!(field(&r, "code"), &Value::Int32(*code), "{cmd}: {r:?}");
                assert_eq!(field(&r, "errmsg"), &Value::from(*msg), "{cmd}: {r:?}");
            }
        }
    }
}

const COMBINE: &str = "Cannot combine timeseries option with other modification options in a single collMod command. Send separate collMod commands for timeseries and other options.";
const SPAN_UP: &str =
    "Timeseries 'bucketMaxSpanSeconds' needs to be equal or greater to transition";
const ROUNDING_UP: &str =
    "Timeseries 'bucketRoundingSeconds' needs to be equal or greater to transition";
const TRANSITION: &str = "Invalid transition for timeseries.granularity. Can only transition from 'seconds' to 'minutes' or 'minutes' to 'hours'.";

#[test]
fn granularity_moves_up_only() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"g","timeseries":{"timeField":"t","metaField":"m"}}"#);
    let seconds = r#"{"timeseries":{"timeField":"t","metaField":"m","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#;
    let hours = r#"{"timeseries":{"timeField":"t","metaField":"m","granularity":"hours","bucketMaxSpanSeconds":2592000,"fixedBucketing":false}}"#;
    check(
        &mut m,
        "g",
        &[
            (r#"{"collMod":"g","timeseries":{"granularity":"seconds"}}"#, Ok(seconds)),
            (r#"{"collMod":"g","timeseries":{}}"#, Ok(seconds)),
            (r#"{"collMod":"g","timeseries":{"granularity":"hours"}}"#, Ok(hours)),
            (r#"{"collMod":"g","timeseries":{"granularity":"minutes"}}"#, Err((72, TRANSITION))),
            (
                r#"{"collMod":"g","timeseries":{"granularity":"days"}}"#,
                Err((
                    2,
                    "Enumeration value 'days' for field 'collMod.timeseries.granularity' is not a valid value.",
                )),
            ),
            (
                r#"{"collMod":"g","timeseries":{"bucketMaxSpanSeconds":100000,"bucketRoundingSeconds":100000}}"#,
                Err((72, SPAN_UP)),
            ),
            (
                r#"{"collMod":"g","timeseries":{"bucketMaxSpanSeconds":300000}}"#,
                Err((
                    72,
                    "Timeseries 'bucketMaxSpanSeconds' and 'bucketRoundingSeconds' need to be set alongside each other",
                )),
            ),
            (
                r#"{"collMod":"g","timeseries":{"timeField":"x"}}"#,
                Err((40415, "BSON field 'collMod.timeseries.timeField' is an unknown field.")),
            ),
            (
                r#"{"collMod":"g","timeseries":{"granularity":"hours","bucketMaxSpanSeconds":300000,"bucketRoundingSeconds":300000}}"#,
                Err((
                    72,
                    "Timeseries 'bucketRoundingSeconds' is not configurable alongside the granularity parameter",
                )),
            ),
            (
                r#"{"collMod":"g","timeseries":{"granularity":"hours","bucketMaxSpanSeconds":300000}}"#,
                Err((
                    72,
                    "Timeseries 'bucketMaxSpanSeconds' is not configurable to a value other than the default for the provided granularity",
                )),
            ),
            (
                r#"{"collMod":"g","timeseries":5}"#,
                Err((
                    14,
                    "BSON field 'collMod.timeseries' is the wrong type 'int', expected type 'object'",
                )),
            ),
            (
                r#"{"collMod":"g","timeseries":{"granularity":"hours"},"expireAfterSeconds":5}"#,
                Err((72, COMBINE)),
            ),
            (
                r#"{"collMod":"g","timeseries":{"granularity":"hours"},"index":{"name":"x","hidden":true}}"#,
                Err((72, COMBINE)),
            ),
            (
                r#"{"collMod":"g","timeseries":{"granularity":"hours"},"dryRun":true}"#,
                Err((72, COMBINE)),
            ),
        ],
    );
}

#[test]
fn custom_buckets_widen() {
    let mut m = Mem::new();
    run(
        &mut m,
        r#"{"create":"h","timeseries":{"timeField":"t","bucketMaxSpanSeconds":100,"bucketRoundingSeconds":100}}"#,
    );
    let custom = |n: i32| {
        format!(
            r#"{{"timeseries":{{"timeField":"t","bucketRoundingSeconds":{n},"bucketMaxSpanSeconds":{n},"fixedBucketing":false}}}}"#
        )
    };
    let (c200, c90000) = (custom(200), custom(90000));
    let minutes = r#"{"timeseries":{"timeField":"t","granularity":"minutes","bucketMaxSpanSeconds":86400,"fixedBucketing":false}}"#;
    check(
        &mut m,
        "h",
        &[
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":200,"bucketRoundingSeconds":200}}"#,
                Ok(&c200),
            ),
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":300,"bucketRoundingSeconds":250}}"#,
                Err((
                    72,
                    "Timeseries 'bucketRoundingSeconds' needs to be equal to 'bucketMaxSpanSeconds'",
                )),
            ),
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":200,"bucketRoundingSeconds":200}}"#,
                Ok(&c200),
            ),
            (r#"{"collMod":"h","timeseries":{"granularity":"seconds"}}"#, Err((72, ROUNDING_UP))),
            (r#"{"collMod":"h","timeseries":{"granularity":"minutes"}}"#, Ok(minutes)),
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":90000,"bucketRoundingSeconds":90000}}"#,
                Ok(&c90000),
            ),
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":"x","bucketRoundingSeconds":5}}"#,
                Err((
                    14,
                    "BSON field 'collMod.timeseries.bucketMaxSpanSeconds' is the wrong type 'string', expected types '[long, int, decimal, double]'",
                )),
            ),
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":0,"bucketRoundingSeconds":0}}"#,
                Err((2, "BSON field 'bucketMaxSpanSeconds' value must be >= 1, actual value '0'")),
            ),
            (
                r#"{"collMod":"h","timeseries":{"bucketMaxSpanSeconds":40000000,"bucketRoundingSeconds":40000000}}"#,
                Err((
                    2,
                    "BSON field 'bucketMaxSpanSeconds' value must be <= 31536000, actual value '40000000'",
                )),
            ),
            (
                r#"{"collMod":"h","timeseries":{"granularity":"minutes","bucketMaxSpanSeconds":86400}}"#,
                Err((72, SPAN_UP)),
            ),
            (r#"{"collMod":"h","timeseries":{"granularity":null}}"#, Ok(&c90000)),
        ],
    );
    run(&mut m, r#"{"create":"k","timeseries":{"timeField":"t","granularity":"minutes"}}"#);
    let k86400 = r#"{"timeseries":{"timeField":"t","bucketRoundingSeconds":86400,"bucketMaxSpanSeconds":86400,"fixedBucketing":false}}"#;
    let hours = r#"{"timeseries":{"timeField":"t","granularity":"hours","bucketMaxSpanSeconds":2592000,"fixedBucketing":false}}"#;
    check(
        &mut m,
        "k",
        &[
            (r#"{"collMod":"k","timeseries":{"granularity":"seconds"}}"#, Err((72, TRANSITION))),
            (
                r#"{"collMod":"k","timeseries":{"bucketMaxSpanSeconds":86400,"bucketRoundingSeconds":86400}}"#,
                Ok(k86400),
            ),
            (r#"{"collMod":"k","timeseries":{"granularity":"hours"}}"#, Ok(hours)),
        ],
    );
}

/// Measurements written before and after the buckets widen are read by time.
#[test]
fn reads_find_the_buckets_of_a_narrower_span() {
    let mut m = Mem::new();
    run(
        &mut m,
        r#"{"create":"w","timeseries":{"timeField":"t","bucketMaxSpanSeconds":200,"bucketRoundingSeconds":200}}"#,
    );
    let date = |s: i64| format!(r#"{{"$date":{{"$numberLong":"{}"}}}}"#, s * 1000);
    run(&mut m, &format!(r#"{{"insert":"w","documents":[{{"t":{},"v":1}}]}}"#, date(350)));
    run(
        &mut m,
        r#"{"collMod":"w","timeseries":{"bucketMaxSpanSeconds":300,"bucketRoundingSeconds":300}}"#,
    );
    run(&mut m, &format!(r#"{{"insert":"w","documents":[{{"t":{},"v":2}}]}}"#, date(320)));
    let r = run(
        &mut m,
        &format!(r#"{{"find":"w","filter":{{"t":{{"$gte":{}}}}},"sort":{{"v":1}}}}"#, date(310)),
    );
    let Value::Array(b) = field(&r, "cursor.firstBatch").clone() else { panic!("{r:?}") };
    let v: Vec<Value> = b
        .iter()
        .map(|d| match d {
            Value::Document(d) => field(d, "v").clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(v, vec![Value::Int32(1), Value::Int32(2)]);
}

#[test]
fn expiry_of_time_series_and_clustered_collections() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"t","timeseries":{"timeField":"t"}}"#);
    let ts = r#"{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}"#;
    let with =
        |n: i64| format!(r#"{{"expireAfterSeconds":{{"$numberLong":"{n}"}},"timeseries":{ts}}}"#);
    let (e2, e6) = (with(2), with(6));
    let without = format!(r#"{{"timeseries":{ts}}}"#);
    check(
        &mut m,
        "t",
        &[
            (r#"{"collMod":"t","expireAfterSeconds":2.7}"#, Ok(&e2)),
            (r#"{"collMod":"t","expireAfterSeconds":{"$numberDecimal":"5.5"}}"#, Ok(&e6)),
            (r#"{"collMod":"t","expireAfterSeconds":"off"}"#, Ok(&without)),
            (r#"{"collMod":"t","expireAfterSeconds":"off"}"#, Ok(&without)),
            (
                r#"{"collMod":"t","expireAfterSeconds":"on"}"#,
                Err((
                    72,
                    "Invalid string value for the 'clusteredIndex::expireAfterSeconds' option. Got: 'on'. Accepted value is 'off'",
                )),
            ),
            (
                r#"{"collMod":"t","expireAfterSeconds":-1}"#,
                Err((72, "TTL index 'expireAfterSeconds' option cannot be less than 0")),
            ),
            (
                r#"{"collMod":"t","expireAfterSeconds":1e30}"#,
                Err((
                    72,
                    "TTL index 'expireAfterSeconds' option cannot overflow int64_t when cast as milliseconds, found 9223372036854775807",
                )),
            ),
            (
                r#"{"collMod":"t","expireAfterSeconds":true}"#,
                Err((
                    14,
                    "BSON field 'collMod.expireAfterSeconds' is the wrong type 'bool', expected types '[string, long]'",
                )),
            ),
        ],
    );
    let r = run(&mut m, r#"{"collMod":"t","expireAfterSeconds":100000000000}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(72), "{r:?}");
    let Value::String(msg) = field(&r, "errmsg") else { panic!("{r:?}") };
    assert!(msg.starts_with(
        "TTL index 'expireAfterSeconds' option cannot exceed time since last epoch ("
    ));
    assert!(msg.ends_with("s) found 100000000000"), "{msg}");
    let r = run(
        &mut m,
        r#"{"create":"t4","timeseries":{"timeField":"t"},"expireAfterSeconds":100000000000}"#,
    );
    let Value::String(msg) = field(&r, "errmsg") else { panic!("{r:?}") };
    assert!(msg.starts_with("Failed to create timeseries collection :: caused by :: TTL index 'expireAfterSeconds' option cannot exceed time since last epoch ("), "{msg}");

    run(
        &mut m,
        r#"{"create":"c","clusteredIndex":{"key":{"_id":1},"unique":true},"expireAfterSeconds":5,"validator":{"a":1}}"#,
    );
    let ci = r#""clusteredIndex":{"v":2,"key":{"_id":1},"name":"_id_","unique":true}"#;
    let c9 =
        format!(r#"{{"validator":{{"a":1}},{ci},"expireAfterSeconds":{{"$numberLong":"9"}}}}"#);
    let c0 = format!(r#"{{"validator":{{"a":1}},{ci}}}"#);
    check(
        &mut m,
        "c",
        &[
            (r#"{"collMod":"c","expireAfterSeconds":"off"}"#, Ok(&c0)),
            (r#"{"collMod":"c","expireAfterSeconds":9}"#, Ok(&c9)),
        ],
    );
    run(&mut m, r#"{"create":"p"}"#);
    run(&mut m, r#"{"create":"cp","capped":true,"size":1000}"#);
    let clustered_only =
        "'expireAfterSeconds' option is only supported on collections clustered by _id";
    let ts_only = "option only supported on a time-series collection: timeseries";
    check(
        &mut m,
        "p",
        &[
            (r#"{"collMod":"p","expireAfterSeconds":5}"#, Err((72, clustered_only))),
            (r#"{"collMod":"cp","expireAfterSeconds":5}"#, Err((72, clustered_only))),
            (r#"{"collMod":"p","timeseries":{"granularity":"hours"}}"#, Err((72, ts_only))),
        ],
    );
}

#[test]
fn views_change_their_definition() {
    let mut m = Mem::new();
    run(&mut m, r#"{"insert":"base","documents":[{"a":1}]}"#);
    run(&mut m, r#"{"create":"w","viewOn":"base","pipeline":[{"$match":{"a":1}}]}"#);
    check(
        &mut m,
        "w",
        &[
            (
                r#"{"collMod":"w","viewOn":"base2"}"#,
                Ok(r#"{"viewOn":"base2","pipeline":[{"$match":{"a":1}}]}"#),
            ),
            (r#"{"collMod":"w","pipeline":[]}"#, Ok(r#"{"viewOn":"base2","pipeline":[]}"#)),
            (r#"{"collMod":"w"}"#, Ok(r#"{"viewOn":"base2","pipeline":[]}"#)),
            (
                r#"{"collMod":"w","viewOn":"base","pipeline":[{"$project":{"a":1}}]}"#,
                Ok(r#"{"viewOn":"base","pipeline":[{"$project":{"a":1}}]}"#),
            ),
            (r#"{"collMod":"w","viewOn":"","pipeline":[]}"#, Err((2, "'viewOn' cannot be empty"))),
            (
                r#"{"collMod":"w","viewOn":5,"pipeline":[]}"#,
                Err((
                    14,
                    "BSON field 'collMod.viewOn' is the wrong type 'int', expected type 'string'",
                )),
            ),
            (
                r#"{"collMod":"w","viewOn":"base","pipeline":{}}"#,
                Err((
                    14,
                    "BSON field 'collMod.pipeline' is the wrong type 'object', expected type 'array'",
                )),
            ),
            (
                r#"{"collMod":"w","viewOn":"base","pipeline":[5]}"#,
                Err((
                    14,
                    "BSON field 'collMod.pipeline.0' is the wrong type 'int', expected type 'object'",
                )),
            ),
            (
                r#"{"collMod":"w","viewOn":"base","pipeline":[{"$out":"x"}]}"#,
                Err((
                    167,
                    "Invalid pipeline for view m.w :: caused by :: The aggregation stage $out in location 0 of the pipeline cannot be used in the view definition of m.w because it writes to disk",
                )),
            ),
            (
                r#"{"collMod":"w","viewOn":"base","pipeline":[],"validator":{"a":1}}"#,
                Err((72, "option not supported on a view: validator")),
            ),
            (
                r#"{"collMod":"w","expireAfterSeconds":5}"#,
                Err((72, "option not supported on a view: expireAfterSeconds")),
            ),
            (
                r#"{"collMod":"w","viewOn":"base","pipeline":[],"index":{"name":"a","hidden":true}}"#,
                Err((72, "option not supported on a view: index")),
            ),
            (
                r#"{"collMod":"w","timeseries":{"granularity":"hours"}}"#,
                Err((72, "option only supported on a time-series collection: timeseries")),
            ),
            (
                r#"{"collMod":"w","viewOn":"w","pipeline":[]}"#,
                Err((5, "View cycle detected: m.w => m.w => m.w")),
            ),
            (
                r#"{"collMod":"base","viewOn":"base2","pipeline":[]}"#,
                Err((72, "option only supported on a view: pipeline")),
            ),
            (
                r#"{"collMod":"base","viewOn":"x"}"#,
                Err((72, "option only supported on a view: viewOn")),
            ),
            (
                r#"{"collMod":"w","viewOn":"nope","pipeline":[{"$match":{"b":1}}]}"#,
                Ok(r#"{"viewOn":"nope","pipeline":[{"$match":{"b":1}}]}"#),
            ),
        ],
    );
    run(&mut m, r#"{"create":"fr","viewOn":"base","pipeline":[],"collation":{"locale":"fr"}}"#);
    check(
        &mut m,
        "w",
        &[(
            r#"{"collMod":"w","viewOn":"fr","pipeline":[]}"#,
            Err((167, "View m.w has conflicting collation with view m.fr")),
        )],
    );
    let r = run(&mut m, r#"{"find":"w"}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}
