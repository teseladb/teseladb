//! The TTL monitor's pass (009 T017, FR-010): a pass at a cluster time deletes the documents of
//! every ready TTL index whose date, or earliest date of an array, plus its seconds is at or
//! before that time; other values and indexes without `expireAfterSeconds` stay.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::ttl::pass;

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn ids(m: &mut Mem) -> Vec<i32> {
    let r = m.run(DB, Document::new().with("find", "c").with("sort", doc(&[("_id", 1.into())])));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .filter_map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::Int32(n)) => Some(*n),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn a_pass_deletes_what_expired_at_its_cluster_time() {
    let mut m = Mem::new();
    let at = |ms: i64| Value::DateTime(ms);
    let docs = vec![
        doc(&[("_id", 1.into()), ("t", at(1_000))]),
        doc(&[("_id", 2.into()), ("t", at(50_000))]),
        doc(&[("_id", 3.into()), ("t", Value::Array(vec![at(90_000), at(500)]))]),
        doc(&[("_id", 4.into()), ("t", 5.into())]),
        doc(&[("_id", 5.into())]),
        doc(&[("_id", 6.into()), ("u", at(0))]),
    ];
    m.run(DB, Document::new().with("insert", "c").with("documents", arr(docs)));
    let ttl = Document::new()
        .with("key", doc(&[("t", 1.into())]))
        .with("name", "t_ttl")
        .with("expireAfterSeconds", 30);
    let plain = Document::new().with("key", doc(&[("u", 1.into())])).with("name", "u_1");
    m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![ttl, plain])));
    let shared = std::mem::take(&mut m.shared);
    // At 40 s the cutoff is 10 s: documents 1 and 3 (earliest date 0.5 s) go.
    assert_eq!(pass(&mut m, &shared, 40_000), Ok(2));
    m.shared = shared;
    assert_eq!(ids(&mut m), [2, 4, 5, 6]);
    let shared = std::mem::take(&mut m.shared);
    assert_eq!(pass(&mut m, &shared, 80_000), Ok(1));
    assert_eq!(pass(&mut m, &shared, 80_000), Ok(0), "nothing left to expire");
    m.shared = shared;
    assert_eq!(ids(&mut m), [4, 5, 6]);
}
