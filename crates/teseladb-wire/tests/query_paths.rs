//! Paths of `find` measured on a `refnode` 9.0.4 on 01-11-2026: the query tree of a tailable read
//! of a collection that is not capped with an equality to a document, `system.views` read in
//! batches of one and with a filter no view matches, and a single batch of `admin.system.users`.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, db: &str, cmd: &str) -> Document {
    m.run(db, json(cmd))
}

fn first_batch(r: &Document) -> &[Value] {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b
}

#[test]
fn a_tailable_read_writes_an_equality_to_a_document_as_eq() {
    let mut m = Mem::new();
    run(&mut m, "q", r#"{"insert":"c","documents":[{"a":{"b":1}}]}"#);
    let r = run(&mut m, "q", r#"{"find":"c","filter":{"a":{"b":1}},"tailable":true}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(2), "{r:?}");
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from(
            "error processing query: ns=q.cTree: a $eq { b: 1 }\nSort: {}\nProj: {}\n tailable cursor requested on non capped collection"
        )
    );
}

#[test]
fn system_views_reads_in_batches_and_through_its_filter() {
    let mut m = Mem::new();
    run(&mut m, "q", r#"{"insert":"c","documents":[{"a":1}]}"#);
    for v in ["v1", "v2"] {
        let r = run(&mut m, "q", &format!(r#"{{"create":"{v}","viewOn":"c","pipeline":[]}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let find = doc(&[("find", Value::from("system.views")), ("batchSize", Value::Int32(1))]);
    let r = m.run("q", find);
    assert_eq!(first_batch(&r).len(), 1, "{r:?}");
    assert!(matches!(field(&r, "cursor.id"), Value::Int64(id) if *id != 0), "{r:?}");
    let r = run(&mut m, "q", r#"{"find":"system.views","filter":{"_id":"q.none"}}"#);
    assert!(first_batch(&r).is_empty(), "{r:?}");
}

#[test]
fn a_single_batch_of_the_users_closes_its_cursor() {
    let mut m = Mem::new();
    for u in ["u1", "u2"] {
        let r = run(&mut m, "admin", &format!(r#"{{"createUser":"{u}","pwd":"p","roles":[]}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let r = run(&mut m, "admin", r#"{"find":"system.users","batchSize":1,"singleBatch":true}"#);
    assert_eq!(first_batch(&r).len(), 1, "{r:?}");
    assert_eq!(field(&r, "cursor.id"), &Value::Int64(0), "{r:?}");
}
