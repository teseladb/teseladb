//! The expressions whose result depends on the operation rather than the document (010 T007,
//! FR-005, FR-006): `$rand`, `$currentDate`, `$createObjectId` and `$meta`, and the one value a
//! reference shell cannot show (`undefined` from `$deserializeEJSON`).

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_exec::expr::{Ctx, Eval, Expr, Scope};

/// 2026-01-01T00:00:00Z.
const NOW: i64 = 1_767_225_600_000;

fn eval_in(e: &str, scope: &Scope) -> Eval {
    let holder =
        extjson::parse(&format!(r#"{{"e":{e}}}"#)).expect("json").to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&holder).expect("a document");
    let expr = Expr::parse_in(raw.get("e").expect("e"), &Ctx::checked([])).expect("parses");
    expr.eval(&Document::new(), scope).unwrap_or_else(|err| panic!("{e}: {err:?}"))
}

fn with_now() -> Scope {
    let mut s = Scope::default().seeded(7);
    s.vars.insert("NOW".to_owned(), Some(Value::DateTime(NOW)));
    s
}

#[test]
fn rand_draws_doubles_in_the_unit_interval_from_the_seed() {
    let scope = Scope::default().seeded(42);
    let draws: Vec<f64> = (0..1000)
        .map(|_| match eval_in(r#"{"$rand":{}}"#, &scope) {
            Some(Value::Double(x)) => x,
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(draws.iter().all(|x| (0.0..1.0).contains(x)));
    let mean = draws.iter().sum::<f64>() / draws.len() as f64;
    assert!((0.4..0.6).contains(&mean), "{mean}");
    // The same seed gives the same draws; scopes made from one share its state.
    let again = Scope::default().seeded(42);
    assert_eq!(eval_in(r#"{"$rand":[]}"#, &again), Some(Value::Double(draws[0])));
    let mapped = eval_in(r#"{"$map":{"input":[1,2,3],"in":{"$rand":{}}}}"#, &scope);
    let Some(Value::Array(xs)) = mapped else { panic!("an array") };
    assert!(xs[0] != xs[1] && xs[1] != xs[2]);
}

#[test]
fn current_date_and_new_object_ids_come_from_the_operation_time() {
    let scope = with_now();
    assert_eq!(eval_in(r#"{"$currentDate":{}}"#, &scope), Some(Value::DateTime(NOW)));
    let Some(Value::ObjectId(a)) = eval_in(r#"{"$createObjectId":{}}"#, &scope) else { panic!() };
    let Some(Value::ObjectId(b)) = eval_in(r#"{"$createObjectId":{}}"#, &scope) else { panic!() };
    assert_eq!(u32::from_be_bytes([a[0], a[1], a[2], a[3]]), (NOW / 1000) as u32);
    assert_ne!(a, b);
    assert_eq!(
        eval_in(r#"{"$toDate":{"$createObjectId":{}}}"#, &scope),
        Some(Value::DateTime(NOW))
    );
}

#[test]
fn meta_reads_the_documents_metadata() {
    let mut scope = Scope::default();
    scope.meta.insert("textScore".to_owned(), Value::Double(1.5));
    scope.meta.insert("recordId".to_owned(), Value::Int64(1));
    assert_eq!(eval_in(r#"{"$meta":"textScore"}"#, &scope), Some(Value::Double(1.5)));
    assert_eq!(eval_in(r#"{"$meta":"recordId"}"#, &scope), Some(Value::Int64(1)));
    assert_eq!(eval_in(r#"{"$meta":"indexKey"}"#, &scope), None);
}

#[test]
fn deserialized_undefined_is_undefined() {
    let e = r#"{"$deserializeEJSON":{"input":{"$arrayToObject":[[[{"$literal":"$undefined"},true]]]}}}"#;
    assert_eq!(eval_in(e, &Scope::default()), Some(Value::Undefined));
}
