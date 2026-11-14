//! 007 T032: TeselaDB's own cluster parameters (D-061): `teselaHistoryRetention` (FR-019, DV-016),
//! the seconds of history every range keeps, and `teselaLoadSplitOpsPerSecond` (D-075), the
//! operations per second that split a range by load. Both go through `setClusterParameter` and
//! `getClusterParameter` like the reference server's own, with whole numbers of at least 1.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::parameters::{
    CHANGE_STREAM_OPTIONS, DEFAULT_HISTORY_SECONDS, DEFAULT_LOAD_SPLIT_OPS, HISTORY_RETENTION,
    LOAD_SPLIT_OPS, history_retention_ms, images_expire_after_ms, load_split_ops,
};

fn set(name: &str, value: Document) -> Document {
    Document::new().with("setClusterParameter", Document::new().with(name, value))
}

fn get(name: &str) -> Document {
    doc(&[("getClusterParameter", Value::from(name))])
}

fn code(r: &Document) -> Option<&Value> {
    r.get("code")
}

#[test]
fn the_defaults_are_a_day_and_2500_operations() {
    let mut m = Mem::new();
    let r = m.run("admin", get(HISTORY_RETENTION));
    let Value::Array(list) = field(&r, "clusterParameters") else { panic!("{r:?}") };
    let Value::Document(p) = &list[0] else { panic!() };
    assert_eq!(p.get("seconds"), Some(&Value::Int64(DEFAULT_HISTORY_SECONDS)));
    assert_eq!(history_retention_ms(None), 86_400_000);
    assert_eq!(load_split_ops(None), DEFAULT_LOAD_SPLIT_OPS as u64);
    let r = m.run("admin", get("*"));
    let Value::Array(list) = field(&r, "clusterParameters") else { panic!("{r:?}") };
    let names: Vec<Value> = list
        .iter()
        .filter_map(|v| match v {
            Value::Document(d) => d.get("_id").cloned(),
            _ => None,
        })
        .collect();
    assert!(
        names.contains(&Value::from(HISTORY_RETENTION))
            && names.contains(&Value::from(LOAD_SPLIT_OPS))
    );
}

#[test]
fn a_value_set_is_stored_and_read_back() {
    let mut m = Mem::new();
    let r = m.run("admin", set(HISTORY_RETENTION, Document::new().with("seconds", 3_600.9)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(history_retention_ms(m.params.get(HISTORY_RETENTION).map(Vec::as_slice)), 3_600_000);
    let r = m.run("admin", set(LOAD_SPLIT_OPS, Document::new().with("opsPerSecond", 40)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(load_split_ops(m.params.get(LOAD_SPLIT_OPS).map(Vec::as_slice)), 40);
    let r = m.run("admin", get(LOAD_SPLIT_OPS));
    let Value::Array(list) = field(&r, "clusterParameters") else { panic!("{r:?}") };
    let Value::Document(p) = &list[0] else { panic!() };
    assert_eq!(p.get("opsPerSecond"), Some(&Value::Int64(40)));
    assert!(matches!(p.get("clusterParameterTime"), Some(Value::Timestamp { .. })));
}

#[test]
fn values_that_are_not_whole_positive_numbers_are_refused() {
    let mut m = Mem::new();
    let prefix = "Failed parsing ClusterServerParameter 'teselaHistoryRetention' :: caused by :: ";
    let cases = [
        (
            Document::new().with("seconds", 0),
            2,
            "BSON field 'seconds' value must be >= 1, actual value '0'",
        ),
        (
            Document::new().with("seconds", "a"),
            14,
            "BSON field 'ClusterServerParameter.seconds' is the wrong type 'string', expected types '[long, int, double, decimal]'",
        ),
        (
            Document::new().with("minutes", 5),
            40415,
            "BSON field 'ClusterServerParameter.minutes' is an unknown field.",
        ),
        (
            Document::new(),
            40414,
            "BSON field 'ClusterServerParameter.seconds' is missing but a required field",
        ),
    ];
    for (value, want, why) in cases {
        let r = m.run("admin", set(HISTORY_RETENTION, value));
        assert_eq!(code(&r), Some(&Value::Int32(want)), "{r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(format!("{prefix}{why}"))), "{r:?}");
    }
    assert!(!m.params.contains_key(HISTORY_RETENTION), "nothing stored");
    let r = m.run("admin", set(LOAD_SPLIT_OPS, Document::new().with("opsPerSecond", -3)));
    assert_eq!(code(&r), Some(&Value::Int32(2)), "{r:?}");
    // A stored value without its field reads as the default.
    let odd = Document::new().with("other", 5).to_bytes().unwrap();
    assert_eq!(history_retention_ms(Some(&odd)), 86_400_000);
    assert_eq!(load_split_ops(Some(&[1, 2])), DEFAULT_LOAD_SPLIT_OPS as u64);
    let int32 = Document::new().with("opsPerSecond", 7).to_bytes().unwrap();
    assert_eq!(load_split_ops(Some(&int32)), 7);
}

/// `changeStreamOptions` with `preAndPostImages.expireAfterSeconds` as the reference router 9.0.4
/// takes it (T046, FR-019; measured on 07-11-2026): `"off"` without a value, a whole number of
/// seconds stored as a `long` (a double truncated, a decimal rounded half to even), and its errors.
#[test]
fn change_stream_options_take_off_or_a_positive_number_as_on_the_reference_router() {
    let mut m = Mem::new();
    let images = |v: Value| {
        Document::new().with("preAndPostImages", Document::new().with("expireAfterSeconds", v))
    };
    let shown = |m: &mut Mem| {
        let r = m.run("admin", get(CHANGE_STREAM_OPTIONS));
        field(&r, "clusterParameters.0.preAndPostImages.expireAfterSeconds").clone()
    };
    assert_eq!(shown(&mut m), Value::from("off"));
    assert_eq!(images_expire_after_ms(None), None);
    let r = m.run("admin", get("*"));
    assert!(format!("{r:?}").contains(CHANGE_STREAM_OPTIONS), "{r:?}");
    for (given, kept) in [
        (Value::Int32(3_600), 3_600),
        (Value::Int64(10), 10),
        (Value::Double(2.7), 2),
        (Value::Decimal128("7.9".parse().unwrap()), 8),
        (Value::Decimal128("2.5".parse().unwrap()), 2),
        (Value::Decimal128("3.5".parse().unwrap()), 4),
    ] {
        let r = m.run("admin", set(CHANGE_STREAM_OPTIONS, images(given.clone())));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{given:?}: {r:?}");
        assert_eq!(shown(&mut m), Value::Int64(kept), "{given:?}");
    }
    let stored = m.params.get(CHANGE_STREAM_OPTIONS).map(Vec::as_slice);
    assert_eq!(images_expire_after_ms(stored), Some(4_000));
    let r = m.run("admin", set(CHANGE_STREAM_OPTIONS, images(Value::from("off"))));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(shown(&mut m), Value::from("off"));
    assert_eq!(
        images_expire_after_ms(m.params.get(CHANGE_STREAM_OPTIONS).map(Vec::as_slice)),
        None
    );

    let prefix = "Failed parsing new changeStreamOptions value";
    let positive = "Numeric value of 'expireAfterSeconds' should be positive".to_owned();
    let refused = [
        (images(Value::Int32(0)), positive.clone()),
        (images(Value::Int32(-5)), positive.clone()),
        (images(Value::Double(0.5)), positive.clone()),
        (images(Value::Decimal128("0.4".parse().unwrap())), positive),
        (
            images(Value::from("never")),
            "Non-numeric value of 'expireAfterSeconds' should be 'off'".into(),
        ),
        (
            images(Value::from("Off")),
            "Non-numeric value of 'expireAfterSeconds' should be 'off'".into(),
        ),
        (
            images(Value::Boolean(true)),
            format!(
                "{prefix}BSON field 'changeStreamOptions.preAndPostImages.expireAfterSeconds' is the wrong type 'bool', expected types '[string, long]'"
            ),
        ),
        (
            Document::new().with("preAndPostImages", Document::new()),
            format!(
                "{prefix}BSON field 'changeStreamOptions.expireAfterSeconds' is missing but a required field"
            ),
        ),
        (
            Document::new(),
            format!(
                "{prefix}BSON field 'changeStreamOptions.preAndPostImages' is missing but a required field"
            ),
        ),
        (
            Document::new().with("other", 1),
            format!(
                "{prefix}BSON field 'changeStreamOptions.preAndPostImages' is missing but a required field"
            ),
        ),
        (
            images(Value::from("off")).with("other", 1),
            format!("{prefix}BSON field 'changeStreamOptions.other' is an unknown field."),
        ),
        (
            Document::new().with(
                "preAndPostImages",
                Document::new().with("expireAfterSeconds", 5).with("x", 1),
            ),
            format!(
                "{prefix}BSON field 'changeStreamOptions.preAndPostImages.x' is an unknown field."
            ),
        ),
        (
            Document::new().with("preAndPostImages", 5),
            format!("{prefix}invalid parameter: expected an object (preAndPostImages)"),
        ),
    ];
    for (value, why) in refused {
        let r = m.run("admin", set(CHANGE_STREAM_OPTIONS, value.clone()));
        assert_eq!(code(&r), Some(&Value::Int32(2)), "{value:?}: {r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(why.as_str())), "{value:?}");
    }
    assert_eq!(shown(&mut m), Value::from("off"), "a refused value changes nothing");
}
