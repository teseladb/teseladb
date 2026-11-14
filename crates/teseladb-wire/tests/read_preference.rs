//! `$readPreference` (007 FR-013) as the reference router 9.0.4 reads it with `find` (measured):
//! the types of its fields, its rules, and where it is checked among the other arguments.

use teseladb_bson::{Document, Value};
use teseladb_router::read_pref::Mode;
use teseladb_wire::error::CommandError;
use teseladb_wire::generic::parse;

fn d(fields: &[(&str, Value)]) -> Document {
    let mut doc = Document::new();
    for (k, v) in fields {
        doc.push(*k, v.clone());
    }
    doc
}

fn s(v: &str) -> Value {
    Value::String(v.to_owned())
}

fn find(rp: Value) -> Document {
    d(&[("find", s("c")), ("$readPreference", rp)])
}

fn err<T: std::fmt::Debug>(r: Result<T, CommandError>) -> (i32, String) {
    let e = r.unwrap_err();
    (e.code.code, e.errmsg)
}

#[test]
fn a_read_preference_is_read_with_its_tags_and_staleness() {
    let tags = Value::Array(vec![
        Value::Document(d(&[("dc", s("east"))])),
        Value::Document(d(&[("dc", Value::Int32(1))])),
        s("x"),
    ]);
    let rp = d(&[
        ("mode", s("secondary")),
        ("maxStalenessSeconds", Value::Int64(100)),
        ("tags", tags),
        ("hedge", Value::Document(d(&[("enabled", Value::Boolean(true))]))),
        ("zz", Value::Int32(1)),
    ]);
    let p = parse(&find(Value::Document(rp)), "find").unwrap().read_pref.unwrap();
    assert_eq!(p.mode, Mode::Secondary);
    assert_eq!(p.max_staleness_ms, Some(100_000));
    assert_eq!(p.tag_sets, vec![Some(vec![("dc".to_owned(), "east".to_owned())]), None, None]);
    assert_eq!(parse(&d(&[("find", s("c"))]), "find").unwrap().read_pref, None);
    // Hedging off is fine with `primary`.
    let off = d(&[
        ("mode", s("primary")),
        ("hedge", Value::Document(d(&[("enabled", Value::Boolean(false))]))),
    ]);
    assert!(parse(&find(Value::Document(off)), "find").is_ok());
}

#[test]
fn bad_fields_get_the_reference_router_codes_and_messages() {
    let rp = |fields: &[(&str, Value)]| find(Value::Document(d(fields)));
    assert_eq!(
        err(parse(&find(s("nearest")), "find")),
        (
            14,
            "BSON field 'find.$readPreference' is the wrong type 'string', expected type 'object'"
                .into()
        )
    );
    assert_eq!(
        err(parse(&rp(&[("tags", Value::Array(vec![]))]), "find")),
        (40414, "BSON field 'readPreference.mode' is missing but a required field".into())
    );
    assert_eq!(
        err(parse(&rp(&[("mode", Value::Int32(1))]), "find")),
        (
            14,
            "BSON field 'readPreference.mode' is the wrong type 'int', expected type 'string'"
                .into()
        )
    );
    assert_eq!(
        err(parse(&rp(&[("mode", s("secondary")), ("maxStalenessSeconds", s("x"))]), "find")),
        (
            14,
            "BSON field 'readPreference.maxStalenessSeconds' is the wrong type 'string', expected \
             types '[int, long, double, decimal]'"
                .into()
        )
    );
    assert_eq!(
        err(parse(&rp(&[("mode", s("secondary")), ("tags", Value::Document(d(&[])))]), "find")),
        (
            14,
            "BSON field 'readPreference.tags' is the wrong type 'object', expected type 'array'"
                .into()
        )
    );
    assert_eq!(
        err(parse(&rp(&[("mode", s("nearest")), ("hedge", Value::Int32(1))]), "find")),
        (
            14,
            "BSON field 'readPreference.hedge' is the wrong type 'int', expected type 'object'"
                .into()
        )
    );
    assert_eq!(
        err(parse(
            &rp(&[("mode", s("secondary")), ("maxStalenessSeconds", Value::Int32(30))]),
            "find"
        )),
        (160, "maxStalenessSeconds value cannot be less than 90".into())
    );
    assert_eq!(
        err(parse(&rp(&[("mode", s("primary")), ("hedge", Value::Document(d(&[])))]), "find")),
        (72, "cannot enable hedging for $readPreference mode \"primaryOnly\"".into())
    );
    assert_eq!(
        err(parse(&rp(&[("mode", s("x"))]), "find")),
        (2, "Enumeration value 'x' for field 'readPreference.mode' is not a valid value.".into())
    );
}

#[test]
fn it_is_checked_in_the_order_of_the_fields() {
    let bad = Value::Document(d(&[("mode", s("x"))]));
    let unknown = (40415, "BSON field 'find.zz' is an unknown field.".to_owned());
    let enum_error = (
        2,
        "Enumeration value 'x' for field 'readPreference.mode' is not a valid value.".to_owned(),
    );
    let first = d(&[("find", s("c")), ("$readPreference", bad.clone()), ("zz", Value::Int32(1))]);
    assert_eq!(err(parse(&first, "find")), enum_error);
    let after = d(&[("find", s("c")), ("zz", Value::Int32(1)), ("$readPreference", bad.clone())]);
    assert_eq!(err(parse(&after, "find")), unknown);
    let fine = Value::Document(d(&[("mode", s("nearest"))]));
    let good_first = d(&[("find", s("c")), ("$readPreference", fine), ("zz", Value::Int32(1))]);
    assert_eq!(err(parse(&good_first, "find")), unknown);
    // A command parsed late reads it before its unknown fields.
    let late = d(&[
        ("listCollections", Value::Int32(1)),
        ("zz", Value::Int32(1)),
        ("$readPreference", bad.clone()),
    ]);
    assert_eq!(err(parse(&late, "listCollections")), enum_error);
    let rc = Value::Document(d(&[("level", s("zz"))]));
    let rc_first = d(&[("find", s("c")), ("readConcern", rc), ("$readPreference", bad)]);
    assert_eq!(
        err(parse(&rc_first, "find")).1,
        "Enumeration value 'zz' for field 'readConcern.level' is not a valid value."
    );
}
