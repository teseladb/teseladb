//! Catalog commands at the edges a `refrouter` 9.0.4 refuses or lists (001 FR-006, 007 FR-017):
//! renames into `config`, a `dropTarget` UUID of another binary subtype, a `create` that asks for
//! other options than the collection has, and the filter of `listCollections` on `config`.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

/// `IllegalOperation`.
const ILLEGAL: i32 = 20;
/// `InvalidUUID`.
const INVALID_UUID: i32 = 207;
/// `NamespaceExists`.
const NAMESPACE_EXISTS: i32 = 48;

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn create(m: &mut Mem, db: &str, coll: &str) {
    let r = m.run(db, doc(&[("create", Value::from(coll))]));
    assert!(ok(&r), "{r:?}");
}

fn rename(from: &str, to: &str, extra: &[(&str, Value)]) -> Document {
    let mut d = doc(&[("renameCollection", Value::from(from)), ("to", Value::from(to))]);
    for (k, v) in extra {
        d.push(*k, v.clone());
    }
    d
}

#[test]
fn a_rename_into_or_out_of_config_and_a_drop_target_of_another_subtype_are_refused() {
    let mut m = Mem::new();
    create(&mut m, "db", "a");
    let r = m.run("admin", rename("db.a", "config.a", &[]));
    assert_eq!(r.get("code"), Some(&Value::Int32(ILLEGAL)), "{r:?}");
    let r = m.run("admin", rename("config.settings", "db.b", &[]));
    assert_eq!(r.get("code"), Some(&Value::Int32(ILLEGAL)), "{r:?}");
    // Sixteen bytes, but of subtype 3 (the old UUID) instead of 4.
    let old = Value::Binary { subtype: 3, bytes: vec![7; 16] };
    let r = m.run("admin", rename("db.a", "db.b", &[("dropTarget", old)]));
    assert_eq!(r.get("code"), Some(&Value::Int32(INVALID_UUID)), "{r:?}");
}

#[test]
fn a_create_that_asks_for_other_options_lists_each_of_them() {
    let mut m = Mem::new();
    create(&mut m, "db", "c");
    let asked = doc(&[
        ("create", Value::from("c")),
        ("capped", Value::Boolean(true)),
        ("size", Value::Int32(4096)),
    ]);
    let r = m.run("db", asked);
    assert_eq!(r.get("code"), Some(&Value::Int32(NAMESPACE_EXISTS)), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(msg.contains("capped: true, size: 4096"), "{msg}");
}

#[test]
fn list_collections_on_config_filters_the_generated_collections() {
    let mut m = Mem::new();
    let filter = doc(&[("name", Value::from("settings"))]);
    let r = m.run(
        "config",
        doc(&[("listCollections", Value::Int32(1)), ("filter", Value::Document(filter))]),
    );
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(batch.len(), 1, "{r:?}");
    assert_eq!(field(&r, "cursor.firstBatch.0.name"), &Value::from("settings"));
}
