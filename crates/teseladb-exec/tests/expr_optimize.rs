//! Expressions as RefDb optimises them when it parses a filter (010 FR-003, FR-005, T018):
//! each `$expr` of `tests/expr/optimize.out`, a tab and what a `refrouter` 9.0.4 gave for it on
//! 01-11-2026 in the plan of an `explain` (`=> {"e": form}`), `=> true` or `=> false` for a
//! constant that settles the filter, or the error (`!! code<TAB>message`) without the words
//! «Failed to optimize expression» the filter adds.

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_exec::expr::optimize::{Optimized, optimize, optimize_filter};
use teseladb_exec::expr::{Ctx, Scope, truthy};

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

fn shown(e: &Value) -> String {
    match optimize(e, &Ctx::default(), &Scope::default()) {
        Err(err) => format!("!! {}\t{}", err.code.code, err.message),
        Ok(Optimized::Const(v)) => format!("=> {}", truthy(&v)),
        Ok(Optimized::Expr(v)) => format!("=> {}", canonical(&Document::new().with("e", v))),
    }
}

#[test]
fn expressions_optimise_as_refdb() {
    let mut wrong = Vec::new();
    for line in include_str!("expr/optimize.out").lines().filter(|l| !l.is_empty()) {
        let (e, want) = line.split_once('\t').expect("an expression and its form");
        let holder = extjson::parse(&format!(r#"{{"e":{e}}}"#)).expect("an expression");
        let got = shown(holder.get("e").expect("e"));
        let want = match want.strip_prefix("=> {") {
            Some(json) => {
                format!("=> {}", canonical(&extjson::parse(&format!("{{{json}")).expect("a form")))
            }
            None => want.to_owned(),
        };
        if got != want {
            wrong.push(format!("{e}\n   got  {got}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

/// The variables a command sets are known when it parses its filter (`let`, `NOW`): a path
/// under one reads its value, and one bound to nothing is nothing.
#[test]
fn known_variables_are_constants() {
    let vars = Document::new().with("v", 5).with("o", Document::new().with("k", "x"));
    let scope = Scope::new(Document::new(), Some(&vars));
    let ctx = Ctx::default();
    let e = |text: &str| -> Value {
        extjson::parse(&format!(r#"{{"e":{text}}}"#)).expect("e").get("e").cloned().expect("e")
    };
    let run = |text: &str| optimize(&e(text), &ctx, &scope).expect("optimised");
    assert_eq!(
        run(r#"{"$eq":["$g","$$v"]}"#),
        Optimized::Expr(e(r#"{"$eq":["$g",{"$const":5}]}"#))
    );
    assert_eq!(run(r#""$$o.k""#), Optimized::Const(Some(Value::String("x".to_owned()))));
    assert_eq!(run(r#""$$v.k""#), Optimized::Const(None));
    assert_eq!(run(r#""$$u""#), Optimized::Expr(e(r#""$$u""#)));
    assert_eq!(Optimized::Const(None).written(), e(r#""$$REMOVE""#));
}

/// Forms the measured expressions leave: each written as RefDb would by the rules above.
#[test]
fn optimisations_beyond_the_measured_forms() {
    let ctx = Ctx::default();
    let vars = Document::new().with("v", 5);
    let scope = Scope::new(Document::new(), Some(&vars));
    let e = |text: &str| -> Value {
        extjson::parse(&format!(r#"{{"e":{text}}}"#)).expect("e").get("e").cloned().expect("e")
    };
    let run = |text: &str| optimize(&e(text), &ctx, &scope).expect("optimised");
    let expr = |text: &str| Optimized::Expr(e(text));
    let konst = |text: &str| Optimized::Const(Some(e(text)));
    assert_eq!(run(r#"{"a":"$$REMOVE","b":1}"#), konst(r#"{"b":1}"#));
    assert_eq!(run(r#"{"$year":{"date":"$d"}}"#), expr(r#"{"$year":{"date":"$d"}}"#));
    assert_eq!(run(r#"{"$year":["$d"]}"#), expr(r#"{"$year":{"date":"$d"}}"#));
    assert_eq!(
        run(r#"{"$filter":{"input":[1,2],"cond":true}}"#),
        expr(r#"{"$filter":{"input":{"$const":[1,2]},"as":"this","cond":{"$const":true}}}"#)
    );
    assert_eq!(run(r#"{"$or":[{"$or":["$a","$b"]},"$c"]}"#), expr(r#"{"$or":["$a","$b","$c"]}"#));
    assert_eq!(run(r#"{"$and":"$a"}"#), expr(r#"{"$and":["$a"]}"#));
    assert_eq!(run(r#"{"$cond":[true,1,"$x"]}"#), konst("1"));
    assert_eq!(run(r#"{"$ifNull":["$a",null,"$b"]}"#), expr(r#"{"$ifNull":["$a","$b"]}"#));
    assert_eq!(
        run(
            r#"{"$switch":{"branches":[{"case":false,"then":1},{"case":"$a","then":2},{"case":true,"then":3}],"default":4}}"#
        ),
        expr(
            r#"{"$switch":{"branches":[{"case":"$a","then":{"$const":2}},{"case":{"$const":true},"then":{"$const":3}}],"default":{"$const":4}}}"#
        )
    );
    assert_eq!(
        run(r#"{"$switch":{"branches":[{"case":false,"then":1}],"default":"$d"}}"#),
        expr(r#""$d""#)
    );
    assert_eq!(
        run(r#"{"$switch":{"branches":[{"case":"$a","then":1}]}}"#),
        expr(r#"{"$switch":{"branches":[{"case":"$a","then":{"$const":1}}]}}"#)
    );
    assert_eq!(
        run(r#"{"$let":{"vars":{"o":{"k":"$$v"}},"in":{"$ifNull":["$$o.z",{"$literal":"$$o"}]}}}"#),
        konst(r#""$$o""#)
    );
    assert_eq!(run(r#"{"$let":{"vars":{"o":"$$v"},"in":{"$add":["$$o.k",1]}}}"#), konst("null"));
    assert_eq!(
        run(r#"{"$let":{"vars":{"o":"$$REMOVE"},"in":{"$ifNull":["$$o","$x"]}}}"#),
        expr(r#""$x""#)
    );
    assert_eq!(run(r#""$$v.k""#), Optimized::Const(None));
    let filter = Document::new().with("$expr", e(r#"{"$eq":[1,1]}"#));
    assert_eq!(optimize_filter(&filter, None).expect("optimised"), Document::new());
}

/// What `optimize_filter` gives for `{$expr: <e>}` with `$$v` known: the filter in relaxed
/// Extended JSON, or the code it fails with.
fn optimized_expr(e: &str) -> Result<String, i32> {
    let vars = teseladb_bson::extjson::parse(r#"{"v":1}"#).expect("vars");
    let f = teseladb_bson::extjson::parse(&format!(r#"{{"$expr":{e}}}"#)).expect("a filter");
    teseladb_exec::expr::optimize::optimize_filter(&f, Some(&vars))
        .map(|d| {
            let bytes = d.to_bytes().expect("small");
            teseladb_bson::extjson::to_relaxed(
                teseladb_bson::RawDoc::from_bytes(&bytes).expect("doc"),
            )
        })
        .map_err(|e| e.code.code)
}

#[test]
fn malformed_operators_reach_their_parse_errors_through_the_optimizer() {
    let kept = |e: &str| Ok(format!(r#"{{"$and":[{{"$expr":{e}}}]}}"#));
    assert_eq!(
        optimized_expr(r#"{"$dateToString":{"date":"$d"}}"#),
        kept(r#"{"$dateToString":{"date":"$d"}}"#)
    );
    assert_eq!(
        optimized_expr(r#"{"$toUpper":[{"$toUpper":["$a"]}]}"#),
        kept(r#"{"$toUpper":[{"$toUpper":["$a"]}]}"#)
    );
    assert_eq!(
        optimized_expr(r#"{"$concat":[{"$concat":"$a"}]}"#),
        kept(r#"{"$concat":[{"$concat":["$a"]}]}"#)
    );
    assert_eq!(
        optimized_expr(r#"{"$ifNull":[null,"$a","$b"]}"#),
        kept(r#"{"$ifNull":["$a","$b"]}"#)
    );
    assert_eq!(
        optimized_expr(r#"{"$switch":{"branches":[{"case":false,"then":1}]}}"#),
        kept(r#"{"$switch":{"branches":[]}}"#)
    );
    assert_eq!(optimized_expr(r#"{"$and":["$$v"]}"#), Ok("{}".to_owned()));
    for (e, code) in [
        (r#"{"$dateToString":["$d"]}"#, 18629),
        (r#"{"$cond":["$a",1]}"#, 16020),
        (r#"{"$switch":5}"#, 40060),
        (r#"{"$switch":{"branches":5,"default":1}}"#, 40061),
        (r#"{"$switch":{"branches":[5,{"case":"$a","then":1}]}}"#, 40062),
        (r#"{"$let":5}"#, 16874),
        (r#"{"$let":{"vars":5,"in":1}}"#, 10065),
    ] {
        assert_eq!(optimized_expr(e), Err(code), "{e}");
    }
}
