//! The `let` of `find`, `update`, `delete`, `findAndModify` and `bulkWrite` (FR-032, T050):
//! checked as a `refrouter` 9.0.4 checks it (measured on 30-10-2026), its literals resolved in 006,
//! its expressions sent to the engine of 010, and its variables carried to the range's leader in
//! the encoded mutation.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::mutation::{DocMutation, Image, Rules};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn error(r: &Document) -> (i32, String) {
    match (field(r, "code"), field(r, "errmsg")) {
        (Value::Int32(c), Value::String(m)) => (*c, m.clone()),
        other => panic!("{other:?} in {r:?}"),
    }
}

/// Each command with `let` set to `v`.
fn commands(v: Value) -> Vec<(&'static str, &'static str, Document)> {
    let stmt = |pairs: &[(&str, Value)]| Value::Array(vec![d(pairs)]);
    vec![
        ("find", "db", doc(&[("find", s("c")), ("let", v.clone())])),
        (
            "update",
            "db",
            doc(&[
                ("update", s("c")),
                ("updates", stmt(&[("q", d(&[])), ("u", d(&[("$set", d(&[("b", 1.into())]))]))])),
                ("let", v.clone()),
            ]),
        ),
        (
            "delete",
            "db",
            doc(&[
                ("delete", s("c")),
                ("deletes", stmt(&[("q", d(&[])), ("limit", 1.into())])),
                ("let", v.clone()),
            ]),
        ),
        (
            "findAndModify",
            "db",
            doc(&[
                ("findAndModify", s("c")),
                ("query", d(&[])),
                ("remove", true.into()),
                ("let", v.clone()),
            ]),
        ),
        (
            "bulkWrite",
            "admin",
            doc(&[
                ("bulkWrite", 1.into()),
                ("ops", stmt(&[("delete", 0.into()), ("filter", d(&[]))])),
                ("nsInfo", stmt(&[("ns", s("db.c"))])),
                ("let", v),
            ]),
        ),
    ]
}

#[test]
fn every_command_checks_its_let() {
    let mut m = Mem::new();
    m.run(
        "db",
        doc(&[("insert", s("c")), ("documents", Value::Array(vec![d(&[("_id", 1.into())])]))]),
    );
    for (name, db, cmd) in commands(5.into()) {
        let want =
            format!("BSON field '{name}.let' is the wrong type 'int', expected type 'object'");
        assert_eq!(error(&m.run(db, cmd)), (14, want), "{name}");
    }
    let bad_name = (
        9,
        "Invalid 'let' parameter :: caused by :: '$v' starts with an invalid character for a user variable name".to_owned(),
    );
    for (name, db, cmd) in commands(d(&[("$v", 1.into())])) {
        assert_eq!(error(&m.run(db, cmd)), bad_name, "{name}");
    }
    for (name, db, cmd) in commands(d(&[("NOW", 1.into())])) {
        let r = m.run(db, cmd);
        assert_eq!(
            error(&r),
            (4_738_901, "Attempt to set internal constant: NOW".into()),
            "{name}"
        );
    }
    for (name, db, cmd) in commands(d(&[("v", s("$a"))])) {
        assert_eq!(error(&m.run(db, cmd)).0, 4_890_500, "{name}");
    }
    // An expression is evaluated by the engine of 010 before the command runs.
    for (name, db, cmd) in
        commands(d(&[("v", d(&[("$add", Value::Array(vec![1.into(), 2.into()]))]))]))
    {
        assert_eq!(field(&m.run(db, cmd), "ok"), &Value::Double(1.0), "{name}");
    }
    for (name, db, cmd) in
        commands(d(&[("v", d(&[("$add", Value::Array(vec![1.into(), s("a")]))]))]))
    {
        assert_eq!(error(&m.run(db, cmd)).0, 14, "{name}");
    }
    // Literal variables are accepted; the commands run.
    for (name, db, cmd) in commands(d(&[("v", 1.into()), ("CURRENT", s("x"))])) {
        assert_eq!(field(&m.run(db, cmd), "ok"), &Value::Double(1.0), "{name}");
    }
}

#[test]
fn the_variables_travel_with_the_mutation() {
    let vars = doc(&[("v", 7.into())]);
    let rules = Rules { vars: Some(vars.clone()), ..Rules::default() };
    let m = DocMutation::Delete { filter: doc(&[]), multi: true, image: Image::None, rules };
    let back = DocMutation::decode(&m.encode().unwrap()).unwrap();
    assert_eq!(back, m);
    let DocMutation::Delete { rules, .. } = back else { panic!("{back:?}") };
    assert_eq!(rules.vars, Some(vars));
}
