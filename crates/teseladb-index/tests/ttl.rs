//! TTL indexes (009 T017, FR-010): which index is one, and which value of its field is expired at
//! a cluster time, as RefDb's TTL monitor decides (a date, or the earliest date of an array,
//! plus `expireAfterSeconds`, at or before now; any other value never expires).

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_index::ttl::{Ttl, expired};

fn spec(key: Document, expire: Option<Value>) -> Vec<u8> {
    let mut d = Document::new().with("v", 2).with("key", key).with("name", "x");
    if let Some(e) = expire {
        d.push("expireAfterSeconds", e);
    }
    d.to_bytes().expect("bytes")
}

fn ttl(key: Document, expire: Option<Value>) -> Option<Ttl> {
    Ttl::of(RawDoc::from_bytes(&spec(key, expire)).expect("doc"))
}

#[test]
fn a_ttl_index_is_one_field_with_expire_after_seconds() {
    let t = ttl(Document::new().with("t", 1), Some(Value::Int32(60)));
    assert_eq!(t, Some(Ttl { field: "t".into(), seconds: 60.0 }));
    assert_eq!(t.map(|t| t.cutoff(100_000)), Some(40_000));
    assert_eq!(
        ttl(Document::new().with("t", -1), Some(Value::Int64(5))).map(|t| t.seconds),
        Some(5.0)
    );
    assert_eq!(ttl(Document::new().with("t", 1), None), None);
    assert_eq!(ttl(Document::new().with("t", 1).with("u", 1), Some(Value::Int32(1))), None);
    assert_eq!(ttl(Document::new().with("t", 1), Some(Value::from("x"))), None);
    assert_eq!(ttl(Document::new().with("t", "2dsphere"), Some(Value::Int32(1))), None);
    let no_key = Document::new().with("expireAfterSeconds", 1).to_bytes().expect("bytes");
    assert_eq!(Ttl::of(RawDoc::from_bytes(&no_key).expect("doc")), None);
}

#[test]
fn a_date_or_the_earliest_date_of_an_array_expires() {
    let value = |v: Value| Document::new().with("v", v).to_bytes().expect("bytes");
    let check = |v: Value, cutoff: i64| {
        let b = value(v);
        expired(RawDoc::from_bytes(&b).expect("doc").get("v").expect("v"), cutoff)
    };
    assert!(check(Value::DateTime(10), 10), "at the cutoff");
    assert!(!check(Value::DateTime(11), 10));
    assert!(check(Value::Array(vec![Value::DateTime(50), Value::DateTime(5)]), 10));
    assert!(!check(Value::Array(vec![Value::DateTime(50), Value::Int32(1)]), 10));
    assert!(!check(Value::Int64(1), 10), "a number is no date");
    assert!(!check(Value::Timestamp { time: 0, increment: 1 }, 10), "nor a timestamp");
}
