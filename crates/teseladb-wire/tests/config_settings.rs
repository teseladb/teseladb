//! 007 FR-017 (contract `config-db.md`): writes to `config.settings` as the reference router 9.0.4
//! takes them (measured, 01-11-2026). Every new version of a document passes the collection's
//! validator, the whole command goes to Group 0 as one change, and a change that finds another one
//! before it starts again.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::config_settings::{
    AUDIT, BALANCER, CHUNK_SIZE, CHUNK_SIZE_RANGE, SETTINGS,
};

const CONFIG: &str = "config";
/// `DocumentValidationFailure`.
const VALIDATION: i32 = 121;
/// `DuplicateKey`.
const DUPLICATE: i32 = 11000;
/// `WriteConflict`.
const CONFLICT: i32 = 112;
/// `IllegalOperation`.
const ILLEGAL: i32 = 20;
/// Attempts of a command before it gives up (`config_settings::MAX_ATTEMPTS`).
const ATTEMPTS: usize = 16;

/// A node of a cluster, whose Group 0 keeps `config.settings`.
fn cluster() -> Mem {
    Mem { settings: Some(Vec::new()), ..Mem::new() }
}

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn chunk(value: i32) -> Value {
    Value::Document(doc(&[("_id", Value::from(CHUNK_SIZE)), ("value", Value::Int32(value))]))
}

fn insert(docs: Vec<Value>, ordered: bool) -> Document {
    doc(&[
        ("insert", Value::from(SETTINGS)),
        ("documents", Value::Array(docs)),
        ("ordered", Value::Boolean(ordered)),
    ])
}

fn update(q: Document, u: Document, upsert: bool) -> Document {
    let st = doc(&[
        ("q", Value::Document(q)),
        ("u", Value::Document(u)),
        ("upsert", Value::Boolean(upsert)),
    ]);
    doc(&[("update", Value::from(SETTINGS)), ("updates", Value::Array(vec![Value::Document(st)]))])
}

fn by_id(id: &str) -> Document {
    doc(&[("_id", Value::from(id))])
}

fn set(field: &str, v: Value) -> Document {
    doc(&[("$set", Value::Document(doc(&[(field, v)])))])
}

fn stored(m: &Mem, id: &str) -> Option<Document> {
    m.settings.iter().flatten().find(|d| d.get("_id") == Some(&Value::from(id))).cloned()
}

#[test]
fn an_insert_keeps_valid_documents_and_reports_the_others() {
    let mut m = cluster();
    let r = m.run(CONFIG, insert(vec![chunk(64)], true));
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(stored(&m, CHUNK_SIZE).unwrap().get("value"), Some(&Value::Int32(64)));
    // Unordered: a balancer mode the validator does not know, the same `_id` again, a value that
    // is no document and a valid balancer document; only the last one goes in.
    let balancer = doc(&[("_id", Value::from(BALANCER)), ("stopped", Value::Boolean(true))]);
    let wrong_mode = doc(&[("_id", Value::from(BALANCER)), ("mode", Value::from("half"))]);
    let docs = vec![
        Value::Document(wrong_mode),
        chunk(8),
        Value::Int32(3),
        Value::Document(balancer.clone()),
    ];
    let r = m.run(CONFIG, insert(docs, false));
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(1)), "{r:?}");
    let codes: Vec<&Value> = (0..3).map(|i| field(&r, &format!("writeErrors.{i}.code"))).collect();
    assert_eq!(codes, [&Value::Int32(VALIDATION), &Value::Int32(DUPLICATE), &Value::Int32(14)]);
    assert_eq!(field(&r, "writeErrors.1.keyValue._id"), &Value::from(CHUNK_SIZE));
    assert_eq!(stored(&m, BALANCER), Some(balancer));
    // Ordered: the first failure stops the command.
    let audit = Value::Document(doc(&[("_id", Value::from(AUDIT)), ("any", Value::Int32(1))]));
    m.settings = Some(Vec::new());
    let r = m.run(CONFIG, insert(vec![chunk(CHUNK_SIZE_RANGE.1 + 1), audit], true));
    assert_eq!(r.get("n"), Some(&Value::Int32(0)), "{r:?}");
    assert!(stored(&m, AUDIT).is_none());
    // A document no branch of the validator takes.
    let other = Value::Document(doc(&[("_id", Value::from("other"))]));
    let r = m.run(CONFIG, insert(vec![other], true));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(VALIDATION));
}

#[test]
fn updates_and_deletes_run_on_the_documents_group_0_keeps() {
    let mut m = cluster();
    m.settings
        .get_or_insert_default()
        .push(doc(&[("_id", Value::from(CHUNK_SIZE)), ("value", Value::Int32(64))]));
    let r = m.run(CONFIG, update(by_id(CHUNK_SIZE), set("value", Value::Int32(128)), false));
    assert!(ok(&r) && r.get("nModified") == Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(stored(&m, CHUNK_SIZE).unwrap().get("value"), Some(&Value::Int32(128)));
    // A new version the validator refuses leaves the document as it was.
    let r = m.run(CONFIG, update(by_id(CHUNK_SIZE), set("value", Value::Int32(0)), false));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(VALIDATION), "{r:?}");
    assert_eq!(stored(&m, CHUNK_SIZE).unwrap().get("value"), Some(&Value::Int32(128)));
    // An upsert creates the balancer's document; an update that changes nothing writes nothing.
    let r = m.run(CONFIG, update(by_id(BALANCER), set("mode", Value::from("off")), true));
    assert!(ok(&r) && field(&r, "upserted.0._id") == &Value::from(BALANCER), "{r:?}");
    m.settings_conflicts = ATTEMPTS;
    let r = m.run(CONFIG, update(by_id(BALANCER), set("mode", Value::from("off")), false));
    assert!(ok(&r) && r.get("nModified") == Some(&Value::Int32(0)), "{r:?}");
    m.settings_conflicts = 0;
    // `findAndModify` and `delete`.
    let fam = doc(&[
        ("findAndModify", Value::from(SETTINGS)),
        ("query", Value::Document(by_id(BALANCER))),
        ("update", Value::Document(set("stopped", Value::Boolean(true)))),
        ("new", Value::Boolean(true)),
    ]);
    let r = m.run(CONFIG, fam);
    assert_eq!(field(&r, "value.stopped"), &Value::Boolean(true), "{r:?}");
    let del = doc(&[("q", Value::Document(by_id(CHUNK_SIZE))), ("limit", Value::Int32(1))]);
    let r = m.run(
        CONFIG,
        doc(&[
            ("delete", Value::from(SETTINGS)),
            ("deletes", Value::Array(vec![Value::Document(del)])),
        ]),
    );
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(1)), "{r:?}");
    assert!(stored(&m, CHUNK_SIZE).is_none());
    // `find` reads them as Group 0 keeps them.
    let r = m.run(CONFIG, doc(&[("find", Value::from(SETTINGS))]));
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::from(BALANCER), "{r:?}");
}

#[test]
fn bypassing_the_validation_lets_any_document_in() {
    let mut m = cluster();
    let mut cmd = insert(vec![chunk(0)], true);
    cmd.push("bypassDocumentValidation", true);
    let r = m.run(CONFIG, cmd);
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(stored(&m, CHUNK_SIZE).unwrap().get("value"), Some(&Value::Int32(0)));
}

#[test]
fn a_change_that_meets_another_one_starts_again_and_gives_up_in_the_end() {
    let mut m = cluster();
    m.settings_conflicts = 2;
    let r = m.run(CONFIG, insert(vec![chunk(32)], true));
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(m.settings_conflicts, 0);
    // A document changed under the command: Group 0 refuses the change, and it is worked out
    // again on the new version.
    m.settings_conflicts = ATTEMPTS;
    let r = m.run(CONFIG, update(by_id(CHUNK_SIZE), set("value", Value::Int32(16)), false));
    assert_eq!(r.get("code"), Some(&Value::Int32(CONFLICT)), "{r:?}");
    assert_eq!(stored(&m, CHUNK_SIZE).unwrap().get("value"), Some(&Value::Int32(32)));
}

#[test]
fn the_config_database_lists_settings_with_its_validator_and_refuses_the_generated_writes() {
    let mut m = cluster();
    let r = m.run(CONFIG, doc(&[("listCollections", Value::Int32(1))]));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let settings = batch
        .iter()
        .find_map(|v| match v {
            Value::Document(d) if d.get("name") == Some(&Value::from(SETTINGS)) => Some(d),
            _ => None,
        })
        .expect("config.settings is listed");
    assert_eq!(field(settings, "options.validationLevel"), &Value::from("strict"));
    assert!(
        matches!(field(settings, "options.validator.$jsonSchema.oneOf"), Value::Array(a) if a.len() == 3)
    );
    let names_only = m.run(
        CONFIG,
        doc(&[("listCollections", Value::Int32(1)), ("nameOnly", Value::Boolean(true))]),
    );
    let Value::Array(names) = field(&names_only, "cursor.firstBatch") else { panic!() };
    assert_eq!(names.len(), batch.len());
    // A collection generated from Group 0 cannot be written.
    let one = Value::Document(doc(&[("_id", Value::from("n9"))]));
    let r = m.run(
        CONFIG,
        doc(&[("insert", Value::from("shards")), ("documents", Value::Array(vec![one]))]),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(ILLEGAL), "{r:?}");
}
