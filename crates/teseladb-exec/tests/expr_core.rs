//! Field paths, variables, comparisons, booleans, conditionals and arithmetic (010 T007, FR-005),
//! each result with the value and type a `refrouter` 9.0.4 gave on 31-10-2026 (`$project` with
//! `$type` beside each expression).

use teseladb_bson::{Decimal128, Document, Value, extjson};
use teseladb_exec::expr::{Expr, Scope, order, try_sort_by};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

/// The document every expression reads.
fn doc() -> Document {
    json(
        r#"{"_id":1,"a":[{"b":1},{"b":[2,3]},{"c":1},5],"n":null,"i":{"$numberInt":"2147483647"},
            "l":{"$numberLong":"9223372036854775807"},"s":"abc",
            "dt":{"$date":{"$numberLong":"1767225600000"}}}"#,
    )
}

/// The result of expression `e` (as extended JSON) on [`doc`].
fn eval(e: &str) -> Option<Value> {
    let holder = json(&format!(r#"{{"e":{e}}}"#)).to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&holder).expect("a document");
    let expr = Expr::parse(raw.get("e").expect("e")).unwrap_or_else(|err| panic!("{e}: {err:?}"));
    let d = doc();
    expr.eval(&d, &Scope::new(d.clone(), None)).unwrap_or_else(|err| panic!("{e}: {err:?}"))
}

fn error(e: &str) -> (i32, String) {
    let holder = json(&format!(r#"{{"e":{e}}}"#)).to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&holder).expect("a document");
    let d = doc();
    match Expr::parse(raw.get("e").expect("e"))
        .and_then(|x| x.eval(&d, &Scope::new(d.clone(), None)))
    {
        Err(err) => (err.code.code, err.message),
        Ok(v) => panic!("{e}: {v:?}"),
    }
}

fn int(n: i32) -> Option<Value> {
    Some(Value::Int32(n))
}

fn long(n: i64) -> Option<Value> {
    Some(Value::Int64(n))
}

fn dbl(n: f64) -> Option<Value> {
    Some(Value::Double(n))
}

fn boolean(b: bool) -> Option<Value> {
    Some(Value::Boolean(b))
}

#[test]
fn paths_and_variables_read_as_refdb_does() {
    assert_eq!(
        eval(r#""$a.b""#),
        Some(Value::Array(vec![
            Value::Int32(1),
            Value::Array(vec![Value::Int32(2), Value::Int32(3)])
        ]))
    );
    assert_eq!(eval(r#""$zz""#), None);
    assert_eq!(eval(r#""$n.x""#), None);
    assert_eq!(eval(r#""$$ROOT._id""#), int(1));
    assert_eq!(eval(r#""$$CURRENT.s""#), Some(Value::from("abc")));
    assert_eq!(eval(r#""$$REMOVE""#), None);
    assert_eq!(eval(r#"{"$literal":"$a"}"#), Some(Value::from("$a")));
    assert_eq!(
        eval(r#"{"x":"$s","y":{"$add":[1,1]},"z":"$zz"}"#),
        Some(Value::Document(json(r#"{"x":"abc","y":2}"#)))
    );
    assert_eq!(
        eval(r#"["$s","$zz",1]"#),
        Some(Value::Array(vec![Value::from("abc"), Value::Null, Value::Int32(1)]))
    );
    assert_eq!(error(r#""$$nope""#).0, 17276);
}

#[test]
fn comparisons_put_a_missing_value_below_null() {
    assert_eq!(eval(r#"{"$eq":["$zz",null]}"#), boolean(false));
    assert_eq!(eval(r#"{"$eq":["$n",null]}"#), boolean(true));
    assert_eq!(eval(r#"{"$lt":["$zz",null]}"#), boolean(true));
    assert_eq!(eval(r#"{"$cmp":["$zz",null]}"#), int(-1));
    assert_eq!(eval(r#"{"$gt":[2,1.5]}"#), boolean(true));
    assert_eq!(eval(r#"{"$eq":[1,1.0]}"#), boolean(true));
    assert_eq!(eval(r#"{"$cmp":["b","a"]}"#), int(1));
    assert_eq!(eval(r#"{"$ne":[1,2]}"#), boolean(true));
    assert_eq!(eval(r#"{"$gte":[1,1]}"#), boolean(true));
    assert_eq!(eval(r#"{"$lte":[2,1]}"#), boolean(false));
    assert_eq!(
        error(r#"{"$eq":[1]}"#),
        (16020, "Expression $eq takes exactly 2 arguments. 1 were passed in.".into())
    );
}

#[test]
fn booleans_and_conditionals_follow_truthiness() {
    assert_eq!(eval(r#"{"$and":[1,"a",[]]}"#), boolean(true));
    assert_eq!(eval(r#"{"$and":[0]}"#), boolean(false));
    assert_eq!(eval(r#"{"$and":[null]}"#), boolean(false));
    assert_eq!(eval(r#"{"$and":["$zz"]}"#), boolean(false));
    assert_eq!(eval(r#"{"$and":[]}"#), boolean(true));
    assert_eq!(eval(r#"{"$or":[0,null,"$zz"]}"#), boolean(false));
    assert_eq!(eval(r#"{"$or":[0,{"$numberDecimal":"1"}]}"#), boolean(true));
    // A decimal too small for a double is not zero (measured on a `refrouter` 9.0.4).
    let tiny = r#"{"$numberDecimal":"1E-6143"}"#;
    assert_eq!(eval(&format!(r#"{{"$and":[{tiny}]}}"#)), boolean(true));
    assert_eq!(eval(&format!(r#"{{"$not":[{tiny}]}}"#)), boolean(false));
    assert_eq!(eval(r#"{"$and":[{"$numberDecimal":"-0E-6176"}]}"#), boolean(false));
    assert_eq!(
        eval(&format!(r#"{{"$divide":[1,{tiny}]}}"#)),
        Some(Value::Decimal128("1.00000000000000000000000000000000E+6143".parse().unwrap()))
    );
    assert_eq!(
        eval(&format!(r#"{{"$mod":[1,{tiny}]}}"#)),
        Some(Value::Decimal128("0E-6143".parse().unwrap()))
    );
    assert_eq!(eval(r#"{"$not":[[]]}"#), boolean(false));
    assert_eq!(eval(r#"{"$cond":[{"$gt":["$i",0]},"y","n"]}"#), Some(Value::from("y")));
    assert_eq!(eval(r#"{"$cond":{"if":false,"then":1,"else":2}}"#), int(2));
    assert_eq!(eval(r#"{"$ifNull":["$zz","$n","d"]}"#), Some(Value::from("d")));
    assert_eq!(eval(r#"{"$ifNull":["$zz","$q"]}"#), None);
    assert_eq!(
        eval(
            r#"{"$switch":{"branches":[{"case":false,"then":1},{"case":true,"then":2}],"default":3}}"#
        ),
        int(2)
    );
    assert_eq!(eval(r#"{"$switch":{"branches":[{"case":false,"then":1}],"default":3}}"#), int(3));
    assert_eq!(error(r#"{"$switch":{"branches":[{"case":false,"then":1}]}}"#), (40069, "Cannot execute a switch statement where all the cases evaluate to false without a default".into()));
    assert_eq!(
        error(r#"{"$cond":{"if":1,"then":2}}"#),
        (17082, "Missing 'else' parameter to $cond".into())
    );
    assert_eq!(error(r#"{"$cond":{"if":1,"then":2,"else":3,"x":4}}"#).0, 17083);
    assert_eq!(error(r#"{"$ifNull":[1]}"#).0, 1257300);
    assert_eq!(error(r#"{"$switch":[]}"#).0, 40060);
    assert_eq!(error(r#"{"$switch":{"branches":1}}"#).0, 40061);
    assert_eq!(error(r#"{"$switch":{"branches":[1]}}"#).0, 40062);
    assert_eq!(error(r#"{"$switch":{"branches":[{"then":1}]}}"#).0, 40064);
    assert_eq!(error(r#"{"$switch":{"branches":[{"case":1}]}}"#).0, 40065);
    assert_eq!(error(r#"{"$switch":{"branches":[{"case":1,"then":1,"x":1}]}}"#).0, 40063);
    assert_eq!(error(r#"{"$switch":{"x":1}}"#).0, 40067);
    assert_eq!(error(r#"{"$switch":{"branches":[]}}"#).0, 40068);
    assert_eq!(error(r#"{"$nope":1}"#), (168, "Unrecognized expression '$nope'".into()));
    assert_eq!(error(r#""$""#).0, 16872);
    assert_eq!(error(r#""$a..b""#).0, 15998);
    assert_eq!(error(r#""$$""#), (9, "empty variable names are not allowed".into()));
    assert_eq!(error(r#"{"$cond":[1,2]}"#).0, 16020);
}

#[test]
fn arithmetic_keeps_refdb_types() {
    assert_eq!(eval(r#"{"$add":["$i",1]}"#), long(2_147_483_648));
    assert_eq!(eval(r#"{"$add":["$l",1]}"#), dbl(9_223_372_036_854_775_808.0));
    assert_eq!(eval(r#"{"$add":["$dt",1000]}"#), Some(Value::DateTime(1_767_225_601_000)));
    // A fraction of a millisecond rounds half away from zero.
    assert_eq!(eval(r#"{"$add":["$dt",1.5]}"#), Some(Value::DateTime(1_767_225_600_002)));
    assert_eq!(eval(r#"{"$add":["$dt",2.5]}"#), Some(Value::DateTime(1_767_225_600_003)));
    assert_eq!(eval(r#"{"$add":["$dt",-1.5]}"#), Some(Value::DateTime(1_767_225_599_998)));
    assert_eq!(eval(r#"{"$mod":[{"$numberLong":"7"},2]}"#), long(1));
    assert_eq!(eval(r#"{"$add":[1,null]}"#), Some(Value::Null));
    assert_eq!(eval(r#"{"$add":[1,"$zz"]}"#), Some(Value::Null));
    assert_eq!(
        eval(r#"{"$add":[1,{"$numberDecimal":"1.5"}]}"#),
        Some(Value::Decimal128("2.5".parse::<Decimal128>().expect("a decimal")))
    );
    assert_eq!(
        eval(r#"{"$subtract":["$dt",{"$date":{"$numberLong":"1767225599000"}}]}"#),
        long(1000)
    );
    assert_eq!(eval(r#"{"$subtract":["$dt",1000]}"#), Some(Value::DateTime(1_767_225_599_000)));
    assert_eq!(eval(r#"{"$multiply":["$i",2]}"#), long(4_294_967_294));
    assert_eq!(eval(r#"{"$divide":[7,2]}"#), dbl(3.5));
    assert_eq!(eval(r#"{"$divide":[6,3]}"#), dbl(2.0));
    assert_eq!(eval(r#"{"$mod":[7,-3]}"#), int(1));
    assert_eq!(eval(r#"{"$mod":[7.5,2]}"#), dbl(1.5));
    assert_eq!(eval(r#"{"$abs":{"$numberInt":"-2147483648"}}"#), long(2_147_483_648));
    assert_eq!(eval(r#"{"$ceil":2.1}"#), dbl(3.0));
    assert_eq!(eval(r#"{"$floor":{"$numberInt":"3"}}"#), int(3));
    assert_eq!(eval(r#"{"$round":[2.5,0]}"#), dbl(2.0));
    assert_eq!(eval(r#"{"$round":[3.5]}"#), dbl(4.0));
    assert_eq!(eval(r#"{"$round":[1234.5678,-2]}"#), dbl(1200.0));
    assert_eq!(eval(r#"{"$trunc":[2.789,1]}"#), dbl(2.7));
    assert_eq!(eval(r#"{"$sqrt":16}"#), dbl(4.0));
    assert_eq!(eval(r#"{"$pow":[2,10]}"#), int(1024));
    assert_eq!(eval(r#"{"$pow":[2,-1]}"#), dbl(0.5));
    assert_eq!(eval(r#"{"$pow":[{"$numberLong":"3"},40]}"#), dbl(3f64.powi(40)));
    assert_eq!(eval(r#"{"$exp":0}"#), dbl(1.0));
    assert_eq!(eval(r#"{"$ln":1}"#), dbl(0.0));
    assert_eq!(eval(r#"{"$log":[100,10]}"#), dbl(2.0));
    assert_eq!(eval(r#"{"$log10":1000}"#), dbl(3.0));
    let dec = |s: &str| Some(Value::Decimal128(s.parse::<Decimal128>().expect("a decimal")));
    assert_eq!(
        eval(r#"{"$divide":[{"$numberDecimal":"1"},3]}"#),
        dec("0.3333333333333333333333333333333333")
    );
    assert_eq!(eval(r#"{"$mod":[{"$numberDecimal":"7.5"},2]}"#), dec("1.5"));
    assert_eq!(eval(r#"{"$round":[{"$numberDecimal":"2.345"},2]}"#), dec("2.34"));
    assert_eq!(eval(r#"{"$trunc":[{"$numberDecimal":"-2.349"},1]}"#), dec("-2.3"));
    assert_eq!(eval(r#"{"$ceil":{"$numberDecimal":"2.1"}}"#), dec("3"));
    assert_eq!(eval(r#"{"$floor":{"$numberDecimal":"-2.1"}}"#), dec("-3"));
    assert_eq!(eval(r#"{"$abs":{"$numberDecimal":"-2.1"}}"#), dec("2.1"));
}

#[test]
fn arithmetic_refuses_what_refdb_refuses() {
    assert_eq!(
        error(r#"{"$add":[1,"a"]}"#),
        (14, "$add only supports numeric or date types, not string".into())
    );
    assert_eq!(error(r#"{"$add":["$dt","$dt"]}"#).0, 16612);
    assert_eq!(error(r#"{"$multiply":[1,"a"]}"#).0, 14);
    assert_eq!(error(r#"{"$subtract":["a",1]}"#), (14, "can't $subtract int from string".into()));
    assert_eq!(error(r#"{"$divide":[1,0]}"#), (2, "can't $divide by zero".into()));
    assert_eq!(
        error(r#"{"$divide":[1,"a"]}"#),
        (14, "$divide only supports numeric types, not int and string".into())
    );
    assert_eq!(error(r#"{"$mod":[1,0]}"#).0, 16610);
    assert_eq!(error(r#"{"$mod":["a",1]}"#).0, 16611);
    assert_eq!(error(r#"{"$abs":"a"}"#).0, 28765);
    assert_eq!(error(r#"{"$sqrt":-1}"#).0, 28714);
    assert_eq!(error(r#"{"$ln":0}"#).0, 28766);
    assert_eq!(error(r#"{"$log10":0}"#).0, 28761);
    assert_eq!(error(r#"{"$log":[0,10]}"#).0, 28758);
    assert_eq!(error(r#"{"$log":[10,1]}"#).0, 28759);
    assert_eq!(error(r#"{"$pow":[0,-1]}"#).0, 28764);
    assert_eq!(
        error(r#"{"$round":[1,200]}"#),
        (51083, "cannot apply $round with precision value 200 value must be in [-20, 100]".into())
    );
    assert_eq!(error(r#"{"$round":[1,1.5]}"#).0, 51082);
    assert_eq!(error(r#"{"$trunc":["a"]}"#).0, 51081);
    assert_eq!(
        error(r#"{"$pow":["a",1]}"#),
        (28762, "$pow's base must be numeric, not string".into())
    );
    assert_eq!(error(r#"{"$abs":{"$numberLong":"-9223372036854775808"}}"#).0, 28680);
    assert_eq!(
        error(r#"{"$switch":{"branches":[1]}}"#).1,
        "$switch expected each branch to be an object, found: int"
    );
    assert_eq!(error(r#"{"$subtract":[1]}"#).0, 16020);
}

/// A value nested `levels` documents deep.
fn nested(levels: usize) -> Value {
    (0..levels).fold(Value::Int32(1), |v, _| Value::Document(Document::new().with("a", v)))
}

/// Values past BSON's depth do not compare: the sort that meets them gives the error.
#[test]
fn sorting_values_that_do_not_compare_gives_the_error() {
    let mut ok = vec![3, 1, 2];
    try_sort_by(&mut ok, |a, b| Ok(a.cmp(b))).expect("comparable");
    assert_eq!(ok, [1, 2, 3]);
    let deep = Some(nested(teseladb_bson::MAX_DEPTH));
    let e = order(&deep, &Some(Value::Int32(1)), None).expect_err("too deep");
    assert_eq!(e.code.code, 2);
    let mut values = vec![deep.clone(), Some(Value::Int32(1))];
    let e = try_sort_by(&mut values, |a, b| order(a, b, None)).expect_err("too deep");
    assert_eq!(e.code.code, 2);
}

#[test]
fn range_refuses_an_array_past_its_memory_limit_before_building_it() {
    // Measured on a `refrouter` 9.0.4 on 11-11-2026: 16 bytes per step and 40 more, against
    // 104857600 bytes; the largest range of 32-bit integers is refused at once.
    let size = |args: &str| eval(&format!(r#"{{"$size":{{"$range":{args}}}}}"#));
    assert_eq!(size("[0, 6553597]"), int(6_553_597));
    assert_eq!(size("[0, 13107195, 2]"), int(6_553_598));
    assert_eq!(size("[10, 0, -1]"), int(10));
    assert_eq!(size("[5, 5]"), int(0));
    let too_much = |args: &str, bytes: u64| {
        let msg = format!(
            "$range would use too much memory ({bytes} bytes) and cannot spill to disk. Memory \
             limit: 104857600 bytes"
        );
        assert_eq!(error(&format!(r#"{{"$range":{args}}}"#)), (146, msg), "{args}");
    };
    too_much("[0, 6553598]", 104_857_608);
    too_much("[0, 13107196, 2]", 104_857_608);
    too_much("[0, -6553599, -1]", 104_857_624);
    too_much("[0, 2147483647]", 34_359_738_392);
    too_much("[-2147483648, 2147483647]", 68_719_476_760);
}

/// A document kept as the bytes it was stored as gives each field path what the same document
/// read into fields gives: its first field of a repeated name, a nested path, a path through an
/// array, and nothing for a field it lacks (H-051 of the native tests: a `$group` reads only its
/// fields from the stored bytes).
#[test]
fn a_stored_document_gives_each_field_path_what_its_fields_give() {
    let mut fields = doc();
    fields.push("s", "second");
    let stored = Document::from_stored(fields.to_bytes().expect("small")).expect("stored");
    assert!(stored.canonical_bytes().is_some(), "kept as its bytes");
    for path in ["$s", "$i", "$l", "$a.b", "$a.c", "$dt", "$n", "$missing", "$a.b.x", "$_id"] {
        let holder = json(&format!(r#"{{"e":"{path}"}}"#)).to_bytes().expect("small");
        let raw = teseladb_bson::RawDoc::from_bytes(&holder).expect("a document");
        let expr = Expr::parse(raw.get("e").expect("e")).expect("a path");
        let read = expr.eval(&fields, &Scope::new(fields.clone(), None)).expect("evaluates");
        let kept = expr.eval(&stored, &Scope::new(stored.clone(), None)).expect("evaluates");
        assert_eq!(kept, read, "{path}");
    }
    let first = Expr::parse(teseladb_bson::RawValue::String("$s")).expect("a path");
    assert_eq!(
        first.eval(&stored, &Scope::new(stored.clone(), None)).expect("evaluates"),
        Some(Value::from("abc")),
        "the first of two fields named s"
    );
}
