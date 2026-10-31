//! The `let` of a command (006 FR-032): RefDb's rules for user variable names and for what a
//! command variable may hold, measured on a `refrouter` 9.0.4 (30-10-2026), and the variables
//! reaching the expression engine of 010 through the integration point.

mod common;

use common::{TestExt, bytes, canon, doc, json, raw};
use teseladb_bson::Value;
use teseladb_mql::vars::resolve_let;
use teseladb_mql::{Filter, FilterUse, MatchContext, NoExtensions};

fn err(let_json: &str) -> (i32, String) {
    match resolve_let(&doc(let_json), &NoExtensions) {
        Err(e) => (e.code.code, e.message.clone()),
        Ok(v) => panic!("{let_json} resolved to {v:?}"),
    }
}

fn parse_error(name: &str, why: &str) -> (i32, String) {
    (9, format!("Invalid 'let' parameter :: caused by :: '{name}' {why}"))
}

#[test]
fn variable_names_follow_refdb_rules() {
    let starts = "starts with an invalid character for a user variable name";
    for name in ["$v", "V", "_v", "9a", "ROOT", "REMOVE", "SEARCH_META"] {
        assert_eq!(err(&format!(r#"{{"{name}": 1}}"#)), parse_error(name, starts), "{name}");
    }
    let contains = |c: char| format!("contains an invalid character for a variable name: '{c}'");
    assert_eq!(err(r#"{"v.x": 1}"#), parse_error("v.x", &contains('.')));
    assert_eq!(err(r#"{"a b": 1}"#), parse_error("a b", &contains(' ')));
    assert_eq!(err(r#"{"a-b": 1}"#), parse_error("a-b", &contains('-')));
    assert_eq!(
        err(r#"{"": 1}"#),
        (9, "Invalid 'let' parameter :: caused by :: empty variable names are not allowed".into())
    );
    for constant in ["NOW", "CLUSTER_TIME", "USER_ROLES", "IS_MR", "JS_SCOPE"] {
        let e = err(&format!(r#"{{"{constant}": 1}}"#));
        assert_eq!(e, (4_738_901, format!("Attempt to set internal constant: {constant}")));
    }
    // Lower-case letters, digits after the first, `_`, non-ASCII letters, and `CURRENT`.
    let ok = resolve_let(&doc(r#"{"a_B9": 1, "é": 2, "CURRENT": 3}"#), &NoExtensions).unwrap();
    assert_eq!(json(&ok), canon(r#"{"a_B9": 1, "é": 2, "CURRENT": 3}"#));
}

#[test]
fn values_may_not_read_documents_and_expressions_go_to_the_engine() {
    let field = (
        4_890_500,
        "Command let Expression tried to access a field, but this is not allowed becauseCommand let Expressions run before the query examines any documents.".to_owned(),
    );
    for v in [r#""$a""#, r#""$$CURRENT.a""#, r#""$$ROOT""#, r#"{"x": "$a"}"#, r#"["$a"]"#] {
        assert_eq!(err(&format!(r#"{{"v": {v}}}"#)), field, "{v}");
    }
    // Literals stay as they are, without the engine.
    let lit = resolve_let(&doc(r#"{"v": {"x": [1, "s"]}, "w": "text"}"#), &NoExtensions).unwrap();
    assert_eq!(json(&lit), canon(r#"{"v": {"x": [1, "s"]}, "w": "text"}"#));
    // An operator or a variable needs the engine of 010.
    let none = resolve_let(&doc(r#"{"v": {"$add": [1, 2]}}"#), &NoExtensions).unwrap_err();
    assert_eq!(none.code.code, 1);
    let e = TestExt::default();
    let r = resolve_let(&doc(r#"{"v": {"$fail": 1}}"#), &e).unwrap_err();
    assert_eq!(r.code.code, 2);
    let r = resolve_let(&doc(r#"{"v": {"$literal": 5}, "w": "$$v"}"#), &e).unwrap();
    assert_eq!(r.get("w"), Some(&Value::Document(doc(r#"{"$literal": 5}"#))));
}

#[test]
fn the_variables_reach_the_engine_with_the_filter() {
    let e = TestExt::default();
    let vars = doc(r#"{"want": true}"#);
    let f = Filter::parse(raw(&bytes(r#"{"$expr": "$$want"}"#)), FilterUse::Query).unwrap();
    let d = bytes(r#"{"a": 1}"#);
    let with = MatchContext { collation: None, ext: &e, vars: Some(&vars) };
    assert!(f.matches(raw(&d), &with).unwrap());
    let without = MatchContext { collation: None, ext: &e, vars: None };
    assert!(!f.matches(raw(&d), &without).unwrap());
}
