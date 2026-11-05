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

fn listed(m: &mut Mem, db: &str, filter: &str) -> Vec<Value> {
    let cmd = format!(r#"{{"listCollections":1,"nameOnly":true,"filter":{filter}}}"#);
    let r = m.run(db, teseladb_bson::extjson::parse(&cmd).expect("command"));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.clone()
}

fn named(name: &str) -> Value {
    Value::Document(doc(&[("name", Value::from(name)), ("type", Value::from("collection"))]))
}

/// The filter of `listCollections` on `local` and on the generated collections of `admin`
/// (measured on a `refnode` 9.0.4 on 03-11-2026).
#[test]
fn list_collections_filters_local_and_the_generated_collections_of_admin() {
    let mut m = Mem::new();
    assert_eq!(listed(&mut m, "local", r#"{"name":"startup_log"}"#), [named("startup_log")]);
    assert_eq!(listed(&mut m, "local", r#"{"name":"none"}"#), Vec::<Value>::new());
    let user = r#"{"createUser":"u1","pwd":"p","roles":[]}"#;
    let r = m.run("admin", teseladb_bson::extjson::parse(user).expect("command"));
    assert!(ok(&r), "{r:?}");
    assert_eq!(listed(&mut m, "admin", r#"{"name":"system.version"}"#), [named("system.version")]);
}

/// `validationLevel: "constraint"` on a collection with no validator, then again (measured on a
/// `refnode` 9.0.4 on 03-11-2026): both succeed, the options show the level and the action.
#[test]
fn constraint_without_a_validator_and_twice() {
    let mut m = Mem::new();
    let run = |m: &mut Mem, cmd: &str| m.run("db", teseladb_bson::extjson::parse(cmd).expect("cmd"));
    assert!(ok(&run(&mut m, r#"{"create":"k","validationLevel":"strict"}"#)));
    run(&mut m, r#"{"insert":"k","documents":[{"a":1}]}"#);
    assert!(ok(&run(&mut m, r#"{"collMod":"k","prepareConstraintValidationLevel":true}"#)));
    for _ in 0..2 {
        let r = run(&mut m, r#"{"collMod":"k","validationLevel":"constraint"}"#);
        assert!(ok(&r), "{r:?}");
        let r = run(&mut m, r#"{"listCollections":1,"filter":{"name":"k"}}"#);
        let want = doc(&[
            ("validationLevel", Value::from("constraint")),
            ("validationAction", Value::from("error")),
        ]);
        assert_eq!(field(&r, "cursor.firstBatch.0.options"), &Value::Document(want), "{r:?}");
    }
}

/// A collection name with a null character is refused by `create`, `insert` and
/// `renameCollection` (measured on a `refnode` 9.0.4 on 03-11-2026).
#[test]
fn namespaces_with_a_null_character_are_refused() {
    let mut m = Mem::new();
    create(&mut m, "nm", "a");
    let msg = Value::from("namespaces cannot have embedded null characters");
    let cmds = [
        ("nm", doc(&[("create", Value::from("a\0b"))])),
        (
            "nm",
            doc(&[
                ("insert", Value::from("a\0b")),
                ("documents", Value::Array(vec![Value::Document(doc(&[]))])),
            ]),
        ),
        ("admin", rename("nm.a", "nm.b\0c", &[])),
    ];
    for (db, cmd) in cmds {
        let r = m.run(db, cmd);
        assert_eq!((field(&r, "code"), field(&r, "errmsg")), (&Value::Int32(73), &msg), "{r:?}");
    }
}
