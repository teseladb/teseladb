//! Buckets spread increasing `_id`s (003 FR-012, T020): a million ObjectIds as a driver makes them
//! (seconds, a fixed process value, a counter) fall into the 16 buckets with less than 5 %
//! deviation from the even share in each one, so that inserts do not all go to the last range.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_keys::{DOCUMENT_BUCKETS, bucket};

#[test]
fn a_million_increasing_object_ids_spread_evenly() {
    const N: u32 = 1_000_000;
    let mut counts = [0u32; DOCUMENT_BUCKETS as usize];
    let process = [0x5E, 0x1A, 0x9C, 0x03, 0x77];
    for i in 0..N {
        let mut oid = [0u8; 12];
        // A thousand inserts a second from 2026-10-30 00:00 UTC.
        oid[..4].copy_from_slice(&(1_793_318_400 + i / 1000).to_be_bytes());
        oid[4..9].copy_from_slice(&process);
        oid[9..].copy_from_slice(&i.to_be_bytes()[1..]);
        let doc = Document::new().with("_id", Value::ObjectId(oid)).to_bytes().unwrap();
        let id = RawDoc::from_bytes(&doc).unwrap().get("_id").unwrap();
        counts[bucket(id, DOCUMENT_BUCKETS) as usize] += 1;
    }
    let even = f64::from(N) / f64::from(DOCUMENT_BUCKETS);
    for (b, c) in counts.iter().enumerate() {
        let deviation = (f64::from(*c) - even).abs() / even;
        assert!(deviation < 0.05, "bucket {b}: {c} ({:.2} %) of {counts:?}", deviation * 100.0);
    }
}
