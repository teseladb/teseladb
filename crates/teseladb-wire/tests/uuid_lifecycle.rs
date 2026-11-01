//! 007 T041 (FR-028): a collection's UUID as `listCollections` shows it, measured on a `refrouter`
//! 9.0.4 on 01-11-2026: it stays through a rename inside its database, changes with a rename to
//! another database (RefDb copies the collection then), and a collection created again gets a
//! new one.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn uuid_of(m: &mut Mem, db: &str, coll: &str) -> Option<Value> {
    let filter = Value::Document(doc(&[("name", Value::from(coll))]));
    let r = m.run(db, doc(&[("listCollections", Value::Int32(1)), ("filter", filter)]));
    let Some(Value::Document(cursor)) = r.get("cursor") else { panic!("{r:?}") };
    let Some(Value::Array(first)) = cursor.get("firstBatch") else { panic!("{r:?}") };
    let Some(Value::Document(c)) = first.first() else { return None };
    let Some(Value::Document(info)) = c.get("info") else { panic!("{c:?}") };
    info.get("uuid").cloned()
}

fn rename(m: &mut Mem, from: &str, to: &str) -> Document {
    let cmd = doc(&[("renameCollection", Value::from(from)), ("to", Value::from(to))]);
    m.run("admin", cmd)
}

#[test]
fn the_uuid_stays_inside_its_database_and_is_new_elsewhere_or_when_created_again() {
    let mut m = Mem::new();
    m.run("u", doc(&[("create", Value::from("a"))]));
    let first = uuid_of(&mut m, "u", "a").expect("a uuid");
    assert!(matches!(first, Value::Binary { subtype: 4, .. }), "{first:?}");
    let r = rename(&mut m, "u.a", "u.b");
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(uuid_of(&mut m, "u", "b"), Some(first.clone()), "same database: same uuid");
    let r = rename(&mut m, "u.b", "v.c");
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let moved = uuid_of(&mut m, "v", "c").expect("a uuid");
    assert_ne!(moved, first, "another database: a new uuid");
    m.run("v", doc(&[("drop", Value::from("c"))]));
    m.run("v", doc(&[("create", Value::from("c"))]));
    let again = uuid_of(&mut m, "v", "c").expect("a uuid");
    assert!(again != moved && again != first, "created again: a new uuid");
}
