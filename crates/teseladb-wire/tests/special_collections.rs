//! `create` of views, capped collections, time series and clustered collections, the shape
//! `listCollections` gives them and the writes a view refuses (015 T005, FR-001 to FR-003, FR-006,
//! FR-009, FR-010), against what the reference router 9.0.4 answered on 31-10-2026.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn create(m: &mut Mem, name: &str, options: &str) -> Document {
    let mut cmd = Document::new().with("create", name);
    for (k, v) in json(options).iter() {
        cmd.push(k, v.clone());
    }
    m.run("m", cmd)
}

fn listed(m: &mut Mem, name: &str) -> Document {
    let r = m.run(
        "m",
        doc(&[
            ("listCollections", 1.into()),
            ("filter", Value::Document(Document::new().with("name", name))),
        ]),
    );
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Array(b)) = c.get("firstBatch") else { panic!("{r:?}") };
    match b.first() {
        Some(Value::Document(d)) => d.clone(),
        _ => panic!("{name} not listed: {r:?}"),
    }
}

fn error(r: &Document) -> Option<(i32, String)> {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => Some((*c, m.clone())),
        _ => None,
    }
}

/// `create` with the options of the first column: the options `listCollections` shows, or the
/// error.
const CREATED: &[(&str, &str)] = &[
    (r#"{"viewOn":"base"}"#, r#"{"viewOn":"base","pipeline":[]}"#),
    (
        r#"{"viewOn":"base","pipeline":[{"$match":{"a":1}}]}"#,
        r#"{"viewOn":"base","pipeline":[{"$match":{"a":1}}]}"#,
    ),
    (r#"{"viewOn":"x","validator":{"a":1}}"#, r#"{"viewOn":"x","pipeline":[]}"#),
    (r#"{"viewOn":"x","validationLevel":"off"}"#, r#"{"viewOn":"x","pipeline":[]}"#),
    (r#"{"capped":true,"size":1000}"#, r#"{"capped":true,"size":1000}"#),
    (r#"{"capped":true,"size":1000,"max":10}"#, r#"{"capped":true,"size":1000,"max":10}"#),
    (r#"{"capped":true,"size":1000,"max":-1}"#, r#"{"capped":true,"size":1000,"max":2147483647}"#),
    (r#"{"capped":true,"size":1000,"max":0}"#, r#"{"capped":true,"size":1000,"max":2147483647}"#),
    (r#"{"capped":true,"size":1000,"max":2.7}"#, r#"{"capped":true,"size":1000,"max":2}"#),
    (r#"{"capped":true,"size":1.5}"#, r#"{"capped":true,"size":1}"#),
    (r#"{"capped":true,"size":2.9}"#, r#"{"capped":true,"size":2}"#),
    (r#"{"capped":1,"size":100}"#, r#"{"capped":true,"size":100}"#),
    (r#"{"capped":true,"size":{"$numberLong":"4096"}}"#, r#"{"capped":true,"size":4096}"#),
    (r#"{"capped":true,"size":{"$numberDecimal":"4096.5"}}"#, r#"{"capped":true,"size":4096}"#),
    (r#"{"capped":true,"size":{"$numberDecimal":"4097.5"}}"#, r#"{"capped":true,"size":4098}"#),
    (r#"{"capped":true,"size":{"$numberDecimal":"4096.7"}}"#, r#"{"capped":true,"size":4097}"#),
    (
        r#"{"max":5,"size":100,"capped":true,"validator":{"a":1}}"#,
        r#"{"capped":true,"size":100,"max":5,"validator":{"a":1}}"#,
    ),
    (
        r#"{"validator":{"a":1},"validationLevel":"moderate","size":100,"capped":true}"#,
        r#"{"capped":true,"size":100,"validator":{"a":1},"validationLevel":"moderate"}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t"}}"#,
        r#"{"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t","metaField":"m","granularity":"hours"}}"#,
        r#"{"timeseries":{"timeField":"t","metaField":"m","granularity":"hours","bucketMaxSpanSeconds":2592000,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"metaField":"m","timeField":"t","granularity":"minutes"}}"#,
        r#"{"timeseries":{"timeField":"t","metaField":"m","granularity":"minutes","bucketMaxSpanSeconds":86400,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":100,"bucketRoundingSeconds":100}}"#,
        r#"{"timeseries":{"timeField":"t","bucketRoundingSeconds":100,"bucketMaxSpanSeconds":100,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":1.5,"bucketRoundingSeconds":1.5}}"#,
        r#"{"timeseries":{"timeField":"t","bucketRoundingSeconds":1,"bucketMaxSpanSeconds":1,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600}}"#,
        r#"{"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"_id"}}"#,
        r#"{"timeseries":{"timeField":"_id","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"expireAfterSeconds":1.5}"#,
        r#"{"expireAfterSeconds":{"$numberLong":"1"},"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true}}"#,
        r#"{"clusteredIndex":{"v":2,"key":{"_id":1},"name":"_id_","unique":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"expireAfterSeconds":{"$numberDouble":"NaN"}}"#,
        r#"{"expireAfterSeconds":{"$numberLong":"0"},"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"collation":{"locale":"simple"}}"#,
        r#"{"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"collation":{"locale":"en","strength":2}}"#,
        r#"{"collation":{"locale":"en","caseLevel":false,"caseFirst":"off","strength":2,"numericOrdering":false,"alternate":"non-ignorable","maxVariable":"punct","normalization":false,"backwards":false,"version":"57.1"},"timeseries":{"timeField":"t","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true}}"#,
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"collation":{"locale":"en","strength":2}}"#,
        r#"{"collation":{"locale":"en","caseLevel":false,"caseFirst":"off","strength":2,"numericOrdering":false,"alternate":"non-ignorable","maxVariable":"punct","normalization":false,"backwards":false,"version":"57.1"},"clusteredIndex":{"v":2,"key":{"_id":1},"name":"_id_","unique":true}}"#,
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":1}}"#,
        r#"{"clusteredIndex":{"v":2,"key":{"_id":1},"name":"_id_","unique":true}}"#,
    ),
    (
        r#"{"expireAfterSeconds":5,"clusteredIndex":{"unique":true,"key":{"_id":1},"name":"k"},"validator":{"a":1}}"#,
        r#"{"validator":{"a":1},"clusteredIndex":{"v":2,"key":{"_id":1},"name":"k","unique":true},"expireAfterSeconds":{"$numberLong":"5"}}"#,
    ),
];

/// `create` with the options of the first column fails with this code and message (`{ns}`:
/// `m.c`).
const REFUSED: &[(&str, i32, &str)] = &[
    (
        r#"{"viewOn":1}"#,
        14,
        "BSON field 'create.viewOn' is the wrong type 'int', expected type 'string'",
    ),
    (
        r#"{"viewOn":"base","pipeline":{}}"#,
        14,
        "BSON field 'create.pipeline' is the wrong type 'object', expected type 'array'",
    ),
    (
        r#"{"viewOn":"x","pipeline":[1]}"#,
        14,
        "BSON field 'create.pipeline.0' is the wrong type 'int', expected type 'object'",
    ),
    (r#"{"pipeline":[]}"#, 72, "'pipeline' requires 'viewOn' to also be specified"),
    (r#"{"viewOn":""}"#, 2, "'viewOn' cannot be empty"),
    (
        r#"{"viewOn":"base","pipeline":[{"$out":"x"}]}"#,
        167,
        "Invalid pipeline for view m.c :: caused by :: The aggregation stage $out in location 0 of the pipeline cannot be used in the view definition of m.c because it writes to disk",
    ),
    (
        r#"{"viewOn":"base","pipeline":[{"$nope":1}]}"#,
        40324,
        "Unrecognized pipeline stage name: '$nope'",
    ),
    (
        r#"{"viewOn":"base","idIndex":{"key":{"_id":1},"name":"_id_"}}"#,
        72,
        r#"'idIndex' is not allowed with 'viewOn': { key: { _id: 1 }, name: "_id_" }"#,
    ),
    (
        r#"{"viewOn":"base","timeseries":{"timeField":"t"}}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'viewOn'",
    ),
    (
        r#"{"viewOn":"base","clusteredIndex":{"key":{"_id":1},"unique":true}}"#,
        6_026_500,
        "The 'clusteredIndex' option is not supported with views",
    ),
    (r#"{"viewOn":"x","capped":true}"#, 72, "specify size:<n> when capped is true"),
    (
        r#"{"viewOn":"x","size":5}"#,
        72,
        "the 'capped' field needs to be true when either the 'size' or 'max' fields are present",
    ),
    (
        r#"{"viewOn":"x","expireAfterSeconds":5}"#,
        72,
        "'expireAfterSeconds' is only supported on time-series collections or when the 'clusteredIndex' option is specified",
    ),
    (
        r#"{"viewOn":"x","changeStreamPreAndPostImages":{"enabled":true}}"#,
        72,
        "option not supported on a view: changeStreamPreAndPostImages",
    ),
    (r#"{"capped":true,"size":0}"#, 2, "BSON field 'size' value must be >= 1, actual value '0'"),
    (r#"{"capped":true,"size":-5}"#, 2, "BSON field 'size' value must be >= 1, actual value '-5'"),
    (
        r#"{"capped":true,"size":{"$numberLong":"1125899906842625"}}"#,
        2,
        "BSON field 'size' value must be <= 1125899906842624, actual value '1125899906842625'",
    ),
    (
        r#"{"capped":true,"size":1000,"max":{"$numberLong":"5000000000"}}"#,
        2,
        "BSON field 'max' value must be < 2147483648, actual value '5000000000'",
    ),
    (
        r#"{"capped":true,"size":"x"}"#,
        14,
        "BSON field 'create.size' is the wrong type 'string', expected types '[double, int, long, decimal]'",
    ),
    (
        r#"{"capped":true,"size":100,"max":"x"}"#,
        14,
        "BSON field 'create.max' is the wrong type 'string', expected types '[double, int, long, decimal]'",
    ),
    (
        r#"{"capped":"yes","size":100}"#,
        14,
        "BSON field 'create.capped' is the wrong type 'string', expected types '[double, decimal, bool, int, long]'",
    ),
    (
        r#"{"capped":true,"size":100,"expireAfterSeconds":5}"#,
        72,
        "'expireAfterSeconds' is only supported on time-series collections or when the 'clusteredIndex' option is specified",
    ),
    (
        r#"{"expireAfterSeconds":5}"#,
        72,
        "'expireAfterSeconds' is only supported on time-series collections or when the 'clusteredIndex' option is specified",
    ),
    (
        r#"{"timeseries":{}}"#,
        40414,
        "BSON field 'create.timeseries.timeField' is missing but a required field",
    ),
    (
        r#"{"timeseries":"x"}"#,
        14,
        "BSON field 'create.timeseries' is the wrong type 'string', expected type 'object'",
    ),
    (
        r#"{"timeseries":{"timeField":1}}"#,
        14,
        "BSON field 'create.timeseries.timeField' is the wrong type 'int', expected type 'string'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","extra":1}}"#,
        40415,
        "BSON field 'create.timeseries.extra' is an unknown field.",
    ),
    (
        r#"{"timeseries":{"timeField":"t","metaField":"t"}}"#,
        72,
        "'metaField' cannot be the same as 'timeField'",
    ),
    (r#"{"timeseries":{"timeField":"t","metaField":"_id"}}"#, 72, "'metaField' cannot be \"_id\""),
    (
        r#"{"timeseries":{"timeField":"a.b"}}"#,
        72,
        "m.c: 'timeField' must be a top-level field and not contain a '.'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","metaField":"a.b"}}"#,
        72,
        "m.c: 'metaField' must be a top-level field and not contain a '.'",
    ),
    (
        r#"{"timeseries":{"timeField":"$a"}}"#,
        2,
        "The 'timeField' or 'metaField' cannot start with '$'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","granularity":"bad"}}"#,
        2,
        "Enumeration value 'bad' for field 'create.timeseries.granularity' is not a valid value.",
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":100}}"#,
        72,
        "Failed to create timeseries collection :: caused by :: Time-series 'bucketRoundingSeconds' must be equal to 'bucketMaxSpanSeconds'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketRoundingSeconds":100}}"#,
        72,
        "Failed to create timeseries collection :: caused by :: Time-series 'bucketRoundingSeconds' must be equal to 'bucketMaxSpanSeconds'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":100,"bucketRoundingSeconds":50}}"#,
        72,
        "Failed to create timeseries collection :: caused by :: Time-series 'bucketRoundingSeconds' must be equal to 'bucketMaxSpanSeconds'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":100,"bucketRoundingSeconds":100,"granularity":"seconds"}}"#,
        72,
        "Failed to create timeseries collection :: caused by :: Time-series 'bucketMaxSpanSeconds' cannot be set to a value other than the default of 3600 for the provided granularity",
    ),
    (
        r#"{"timeseries":{"timeField":"t","granularity":"minutes","bucketMaxSpanSeconds":86400,"bucketRoundingSeconds":60}}"#,
        72,
        "Failed to create timeseries collection :: caused by :: Time-series 'bucketRoundingSeconds' cannot be set alongside the granularity parameter",
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":0,"bucketRoundingSeconds":0}}"#,
        2,
        "BSON field 'bucketMaxSpanSeconds' value must be >= 1, actual value '0'",
    ),
    (
        r#"{"timeseries":{"timeField":"t","bucketMaxSpanSeconds":31536001,"bucketRoundingSeconds":31536001}}"#,
        2,
        "BSON field 'bucketMaxSpanSeconds' value must be <= 31536000, actual value '31536001'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"expireAfterSeconds":-1}"#,
        72,
        "Failed to create timeseries collection :: caused by :: TTL index 'expireAfterSeconds' option cannot be less than 0",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"expireAfterSeconds":"off"}"#,
        14,
        "BSON field 'create.expireAfterSeconds' is the wrong type 'string', expected types '[double, int, long, decimal]'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"capped":true,"size":100}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'capped'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"clusteredIndex":{"key":{"_id":1},"unique":true}}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'clusteredIndex'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"validator":{"a":1}}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'validator'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"validationLevel":"off"}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'validationLevel'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"changeStreamPreAndPostImages":{"enabled":true}}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'changeStreamPreAndPostImages'",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"idIndex":{"key":{"_id":1},"name":"_id_"}}"#,
        72,
        "m.c: 'timeseries' is not allowed with 'idIndex'",
    ),
    (
        r#"{"clusteredIndex":true}"#,
        5_979_703,
        "The 'clusteredIndex' legacy format {clusteredIndex: <bool>} is only supported for specific internal collections and vice versa",
    ),
    (
        r#"{"clusteredIndex":{"key":{"a":1},"unique":true}}"#,
        197,
        "The clusteredIndex option is only supported for key: {_id: 1}",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1,"a":1},"unique":true}}"#,
        197,
        "The clusteredIndex option is only supported for key: {_id: 1}",
    ),
    (
        r#"{"timeseries":{"timeField":"t"},"expireAfterSeconds":1e30}"#,
        72,
        "Failed to create timeseries collection :: caused by :: TTL index 'expireAfterSeconds' option cannot overflow int64_t when cast as milliseconds, found 9223372036854775807",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"expireAfterSeconds":1e30}"#,
        72,
        "TTL index 'expireAfterSeconds' option cannot overflow int64_t when cast as milliseconds, found 9223372036854775807",
    ),
    (
        r#"{"capped":true,"size":{"$numberDouble":"NaN"}}"#,
        2,
        "BSON field 'size' value must be >= 1, actual value '0'",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":-1},"unique":true}}"#,
        197,
        "The clusteredIndex option is only supported for key: {_id: 1}",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":false}}"#,
        5_979_700,
        "The clusteredIndex option requires unique: true to be specified",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1}}}"#,
        40414,
        "BSON field 'create.clusteredIndex.unique' is missing but a required field",
    ),
    (
        r#"{"clusteredIndex":{"unique":true}}"#,
        40414,
        "BSON field 'create.clusteredIndex.key' is missing but a required field",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true,"extra":1}}"#,
        40415,
        "BSON field 'create.clusteredIndex.extra' is an unknown field.",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true,"v":1}}"#,
        5_979_704,
        r#"Invalid clusteredIndex specification { v: 1, key: { _id: 1 }, name: "_id_", unique: true }; cannot create a clusteredIndex with v=1"#,
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true,"name":""}}"#,
        67,
        "The clusteredIndex name cannot be empty",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"capped":true}"#,
        6_127_800,
        "Clustered capped collection only available with 'enableTestCommands' server parameter",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"capped":true,"size":100}"#,
        6_049_200,
        "'size' field for capped collections is not allowed on clustered collections. Did you mean 'capped: true' with 'expireAfterSeconds'?",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"idIndex":{"key":{"_id":1},"name":"_id_"}}"#,
        72,
        "The 'clusteredIndex' option is not supported with the 'idIndex' option",
    ),
    (
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"expireAfterSeconds":-1}"#,
        72,
        "TTL index 'expireAfterSeconds' option cannot be less than 0",
    ),
];

#[test]
fn create_stores_the_options_of_each_kind_as_the_reference_router_does() {
    for (options, shown) in CREATED {
        let mut m = Mem::new();
        let r = create(&mut m, "c", options);
        assert_eq!(error(&r), None, "{options}");
        let listed = listed(&mut m, "c");
        assert_eq!(listed.get("options"), Some(&Value::Document(json(shown))), "{options}");
    }
}

#[test]
fn create_refuses_the_options_the_reference_router_refuses() {
    for (options, code, msg) in REFUSED {
        let mut m = Mem::new();
        let r = create(&mut m, "c", options);
        assert_eq!(error(&r), Some((*code, (*msg).to_owned())), "{options}");
    }
}

#[test]
fn list_collections_shows_each_kind_with_its_type_and_info() {
    let mut m = Mem::new();
    create(&mut m, "base", "{}");
    create(&mut m, "v", r#"{"viewOn":"base","pipeline":[{"$match":{"a":1}}]}"#);
    create(&mut m, "ts", r#"{"timeseries":{"timeField":"t","metaField":"m"}}"#);
    create(&mut m, "cl", r#"{"clusteredIndex":{"key":{"_id":1},"unique":true}}"#);
    create(&mut m, "cap", r#"{"capped":true,"size":1000}"#);
    let v = listed(&mut m, "v");
    assert_eq!(v.get("type"), Some(&Value::from("view")));
    assert_eq!(v.get("info"), Some(&Value::Document(Document::new().with("readOnly", true))));
    assert_eq!(v.get("idIndex"), None);
    assert_eq!(field(&v, "options.pipeline.0.$match.a"), &Value::Int32(1));
    let ts = listed(&mut m, "ts");
    assert_eq!(ts.get("type"), Some(&Value::from("timeseries")));
    assert_eq!(ts.get("idIndex"), None);
    let cl = listed(&mut m, "cl");
    assert_eq!(cl.get("type"), Some(&Value::from("collection")));
    assert_eq!(cl.get("idIndex"), None);
    let cap = listed(&mut m, "cap");
    assert!(cap.get("idIndex").is_some());
    // The first view of a database brings `system.views`, which stays after its views go.
    let sv = listed(&mut m, "system.views");
    assert_eq!(sv.get("type"), Some(&Value::from("collection")));
    assert!(sv.get("idIndex").is_some());
    m.run("m", Document::new().with("drop", "v"));
    listed(&mut m, "system.views");
    // Dropping `system.views` drops the views of its database.
    create(&mut m, "v2", r#"{"viewOn":"base"}"#);
    m.run("m", Document::new().with("drop", "system.views"));
    let r = m.run("m", json(r#"{"listCollections":1,"filter":{"type":"view"}}"#));
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
    assert_eq!(c.get("firstBatch"), Some(&Value::Array(Vec::new())));
}

#[test]
fn system_views_holds_the_definitions_and_refuses_writes() {
    let mut m = Mem::new();
    create(&mut m, "v", r#"{"viewOn":"b","pipeline":[{"$match":{"a":1}}]}"#);
    let r = m.run("m", Document::new().with("find", "system.views"));
    let want = json(r#"{"_id":"m.v","viewOn":"b","pipeline":[{"$match":{"a":1}}]}"#);
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
    assert_eq!(c.get("firstBatch"), Some(&Value::Array(vec![Value::Document(want)])));
    create(&mut m, "w", r#"{"viewOn":"b"}"#);
    let r = m.run("m", json(r#"{"find":"system.views","filter":{"viewOn":"b"},"skip":1,"limit":1,"projection":{"_id":1}}"#));
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
    let want = Value::Document(Document::new().with("_id", "m.w"));
    assert_eq!(c.get("firstBatch"), Some(&Value::Array(vec![want])));
    let r = m.run("m", json(r#"{"insert":"system.views","documents":[{"x":1}]}"#));
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!("{r:?}") };
    assert_eq!(e.get("code"), Some(&Value::Int32(73)));
}

#[test]
fn a_repeated_create_compares_the_kind() {
    let mut m = Mem::new();
    create(&mut m, "v", r#"{"viewOn":"b","pipeline":[{"$match":{"a":1}}]}"#);
    create(&mut m, "t", r#"{"timeseries":{"timeField":"t"}}"#);
    create(&mut m, "k", r#"{"capped":true,"size":100}"#);
    let cases: &[(&str, &str, Option<(i32, &str)>)] = &[
        ("v", r#"{"viewOn":"b","pipeline":[{"$match":{"a":1}}]}"#, None),
        ("v", "{}", Some((48, "namespace 'm.v' already exists, but is a view"))),
        (
            "v",
            r#"{"viewOn":"z","pipeline":[{"$match":{"a":1}}]}"#,
            Some((48, "namespace 'm.v' already exists, but is a view on 'm.b' rather than 'm.z'")),
        ),
        (
            "v",
            r#"{"viewOn":"b","pipeline":[{"$match":{"a":2}}]}"#,
            Some((
                48,
                "namespace 'm.v' already exists, but with pipeline { 0: { $match: { a: 1 } } } rather than { 0: { $match: { a: 2 } } }",
            )),
        ),
        (
            "v",
            r#"{"viewOn":"b","pipeline":[{"$match":{"a":1}}],"collation":{"locale":"fr"}}"#,
            Some((
                48,
                r#"namespace 'm.v' already exists, but with collation: {} rather than { locale: "fr", caseLevel: false, caseFirst: "off", strength: 3, numericOrdering: false, alternate: "non-ignorable", maxVariable: "punct", normalization: false }"#,
            )),
        ),
        (
            "t",
            r#"{"timeseries":{"timeField":"u"}}"#,
            Some((
                48,
                r#"namespace 'm.t' already exists, but with different options: { timeseries: { timeField: "t", granularity: "seconds", bucketMaxSpanSeconds: 3600, fixedBucketing: true } }, requested: { timeseries: { timeField: "u", granularity: "seconds", bucketMaxSpanSeconds: 3600, fixedBucketing: true } }"#,
            )),
        ),
        (
            "t",
            r#"{"viewOn":"b"}"#,
            Some((48, "namespace 'm.t' already exists, but is a collection rather than a view")),
        ),
        ("t", r#"{"timeseries":{"timeField":"t","granularity":"seconds"}}"#, None),
        ("k", r#"{"capped":true,"size":100}"#, None),
    ];
    for (name, options, want) in cases {
        let r = create(&mut m, name, options);
        assert_eq!(error(&r), want.map(|(c, s)| (c, s.to_owned())), "{name} {options}");
    }
}

#[test]
fn a_view_refuses_writes_and_index_commands() {
    let mut m = Mem::new();
    create(&mut m, "v", r#"{"viewOn":"b"}"#);
    let msg = "Namespace m.v is a view, not a collection";
    let one_error = |r: &Document| {
        let Some(Value::Array(e)) = r.get("writeErrors") else { panic!("{r:?}") };
        e.iter()
            .map(|e| match e {
                Value::Document(e) => (e.get("code").cloned(), e.get("errmsg").cloned()),
                _ => panic!("{r:?}"),
            })
            .collect::<Vec<_>>()
    };
    let want = (Some(Value::Int32(166)), Some(Value::from(msg)));
    for cmd in [
        r#"{"insert":"v","documents":[{"a":1}]}"#,
        r#"{"update":"v","updates":[{"q":{},"u":{"$set":{"a":1}}}]}"#,
        r#"{"delete":"v","deletes":[{"q":{},"limit":0}]}"#,
    ] {
        let r = m.run("m", json(cmd));
        assert_eq!(one_error(&r), vec![want.clone()], "{cmd}");
    }
    let r = m.run("m", json(r#"{"insert":"v","documents":[{"x":1},{"x":2}],"ordered":false}"#));
    assert_eq!(one_error(&r), vec![want.clone(), want.clone()]);
    for cmd in [
        r#"{"findAndModify":"v","query":{},"remove":true}"#,
        r#"{"createIndexes":"v","indexes":[{"key":{"a":1},"name":"a_1"}]}"#,
        r#"{"listIndexes":"v"}"#,
    ] {
        let r = m.run("m", json(cmd));
        assert_eq!(error(&r), Some((166, msg.to_owned())), "{cmd}");
    }
}

#[test]
fn a_view_definition_is_checked_against_the_other_views() {
    let mut m = Mem::new();
    create(&mut m, "fr", r#"{"viewOn":"b","collation":{"locale":"fr"}}"#);
    let r = create(&mut m, "onfr", r#"{"viewOn":"fr"}"#);
    assert_eq!(
        error(&r),
        Some((167, "View m.onfr has conflicting collation with view m.fr".to_owned()))
    );
    let r = create(&mut m, "self", r#"{"viewOn":"self"}"#);
    assert_eq!(error(&r), Some((5, "View cycle detected: m.self => m.self => m.self".to_owned())));
}

fn first_batch(r: &Document) -> Vec<Value> {
    let Some(Value::Document(c)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Array(b)) = c.get("firstBatch") else { panic!("{r:?}") };
    b.clone()
}

#[test]
fn a_clustered_collection_keeps_its_documents_in_id_order() {
    let mut m = Mem::new();
    create(&mut m, "cl", r#"{"clusteredIndex":{"key":{"_id":1},"unique":true}}"#);
    create(&mut m, "plain", "{}");
    let ids: Vec<i32> = (0..40).map(|i| (i * 17) % 40).collect();
    for name in ["cl", "plain"] {
        let docs: Vec<Value> =
            ids.iter().map(|i| Value::Document(Document::new().with("_id", *i))).collect();
        m.run("m", Document::new().with("insert", name).with("documents", docs));
    }
    let order = |m: &mut Mem, name: &str| -> Vec<Value> {
        let r = m.run("m", Document::new().with("find", name).with("batchSize", 100));
        first_batch(&r)
    };
    let sorted: Vec<Value> =
        (0..40).map(|i| Value::Document(Document::new().with("_id", i))).collect();
    assert_eq!(order(&mut m, "cl"), sorted);
    // Without clustering the documents are spread by the hash of their `_id`.
    assert_ne!(order(&mut m, "plain"), sorted);
    let r = m.run("m", json(r#"{"find":"cl","filter":{"_id":7}}"#));
    assert_eq!(first_batch(&r), vec![Value::Document(Document::new().with("_id", 7))]);
}

#[test]
fn the_ttl_monitor_expires_a_clustered_collection_by_its_id() {
    let mut m = Mem::new();
    create(
        &mut m,
        "cl",
        r#"{"clusteredIndex":{"key":{"_id":1},"unique":true},"expireAfterSeconds":10}"#,
    );
    let now = i64::try_from(m.now).expect("a time");
    let docs: Vec<Value> = [now - 20_000, now - 5_000]
        .into_iter()
        .map(|t| Value::Document(Document::new().with("_id", Value::DateTime(t))))
        .chain([Value::Document(Document::new().with("_id", 1))])
        .collect();
    m.run("m", Document::new().with("insert", "cl").with("documents", docs));
    let shared = std::mem::take(&mut m.shared);
    let deleted = teseladb_wire::commands::ttl::pass(&mut m, &shared, now).expect("a pass");
    m.shared = shared;
    assert_eq!(deleted, 1);
    let r = m.run("m", Document::new().with("find", "cl"));
    assert_eq!(first_batch(&r).len(), 2);
}

#[test]
fn a_repeated_create_shows_the_clustering_of_a_time_series() {
    let mut m = Mem::new();
    create(&mut m, "t", r#"{"timeseries":{"timeField":"t"}}"#);
    let r = create(&mut m, "t", "{}");
    let (code, msg) = error(&r).expect("an error");
    assert_eq!(code, 48);
    assert!(
        msg.starts_with(
            "namespace 'm.t' already exists, but with different options: { uuid: UUID(\""
        ),
        "{msg}"
    );
    assert!(msg.ends_with("\"), clusteredIndex: true, timeseries: { timeField: \"t\", granularity: \"seconds\", bucketMaxSpanSeconds: 3600, fixedBucketing: true } }, requested: {}"), "{msg}");
}
