//! The extension point of server-side JavaScript (010 T012, FR-019): `$function` and
//! `$accumulator` call the engine the environment gives (016 provides it), here a stand-in that
//! knows the functions of the measured pipelines, whose results a `refrouter` 9.0.4 gave as
//! `y: 10.0, 6.0` and `t: 8.0, 80.0`.

use std::rc::Rc;

use teseladb_bson::{Document, RawValue, Value, extjson};
use teseladb_exec::expr::js::JsEngine;
use teseladb_exec::expr::{Ctx, Scope};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

/// The functions of the measured pipelines, as JavaScript's numbers (doubles).
#[derive(Debug)]
struct Stand;

fn number(v: &Value) -> f64 {
    match v {
        Value::Int32(n) => f64::from(*n),
        Value::Int64(n) => *n as f64,
        Value::Double(x) => *x,
        _ => f64::NAN,
    }
}

impl JsEngine for Stand {
    fn call(&self, body: &Value, args: &[Value]) -> Result<Value, MqlError> {
        let Value::String(code) = body else { return Ok(Value::Null) };
        let first = args.first().map_or(0.0, number);
        Ok(Value::Double(match code.as_str() {
            "function(a){return a*2}" => first * 2.0,
            "function(){return 0}" => 0.0,
            "function(s,n){return s+1}" => first + 1.0,
            "function(a,b){return a+b}" => first + args.get(1).map_or(0.0, number),
            "function(s){return s*10}" => first * 10.0,
            _ => return Ok(Value::Null),
        }))
    }
}

struct JsEnv {
    docs: Vec<Document>,
}

impl Env for JsEnv {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
        Ok(self.docs.clone())
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn js(&self) -> Option<Rc<dyn JsEngine>> {
        Some(Rc::new(Stand))
    }
}

fn run(pipeline: &str) -> Vec<String> {
    let holder = extjson::parse(&format!(r#"{{"p":{pipeline}}}"#)).expect("pipeline");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    let p = Pipeline::parse(&stages, &Ctx::default()).expect("parses");
    let docs: Vec<Document> = (1..=8)
        .map(|i| {
            Document::new().with("_id", i).with("n", [5, 3, 7, 2, -1, 10, 0, 5][i as usize - 1])
        })
        .collect();
    let env = JsEnv { docs: docs.clone() };
    p.run(docs, &env)
        .expect("runs")
        .iter()
        .map(|d| {
            let b = d.to_bytes().expect("small");
            extjson::to_canonical(teseladb_bson::RawDoc::from_bytes(&b).expect("a document"))
        })
        .collect()
}

#[test]
fn functions_run_in_the_engine_of_the_environment() {
    assert_eq!(
        run(
            r#"[{"$project":{"y":{"$function":{"body":"function(a){return a*2}","args":["$n"],"lang":"js"}}}},{"$limit":2}]"#
        ),
        [
            r#"{"_id":{"$numberInt":"1"},"y":{"$numberDouble":"10.0"}}"#,
            r#"{"_id":{"$numberInt":"2"},"y":{"$numberDouble":"6.0"}}"#
        ]
    );
}

#[test]
fn accumulators_run_init_accumulate_and_finalize_in_the_engine() {
    let base = r#""init":"function(){return 0}","accumulate":"function(s,n){return s+1}","accumulateArgs":["$n"],"merge":"function(a,b){return a+b}","lang":"js""#;
    assert_eq!(
        run(&format!(r#"[{{"$group":{{"_id":null,"t":{{"$accumulator":{{{base}}}}}}}}}]"#)),
        [r#"{"_id":null,"t":{"$numberDouble":"8.0"}}"#]
    );
    assert_eq!(
        run(&format!(
            r#"[{{"$group":{{"_id":null,"t":{{"$accumulator":{{{base},"finalize":"function(s){{return s*10}}","initArgs":[]}}}}}}}}]"#
        )),
        [r#"{"_id":null,"t":{"$numberDouble":"80.0"}}"#]
    );
}

#[test]
fn partial_states_merge_in_the_engine() {
    let spec = extjson::parse(
        r#"{"a":{"init":"function(){return 0}","accumulate":"function(s,n){return s+1}","accumulateArgs":[],"merge":"function(a,b){return a+b}","lang":"js"}}"#,
    )
    .expect("spec");
    let bytes = spec.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let acc =
        teseladb_exec::expr::js::Accumulator::parse_spec(raw.get("a").expect("a"), &Ctx::default())
            .expect("parses");
    let scope = Scope::default().with_js(Some(Rc::new(Stand)));
    let merged = acc.merge_states(Value::Double(3.0), Value::Double(5.0), &scope).expect("merges");
    assert_eq!(merged, Value::Double(8.0));
    let none = acc.merge_states(Value::Double(1.0), Value::Double(1.0), &Scope::default());
    assert!(none.is_err(), "no engine");
}
