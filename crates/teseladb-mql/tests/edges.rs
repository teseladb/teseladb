//! Edge cases across the crate: decimal arithmetic of `$inc` and `$mul`, unusual paths of the
//! update walk, rare regular expression syntax, messages and the integration points' defaults.

pub mod common;

use common::{TestExt, bytes, canon, json, raw};
use teseladb_mql::projection::{Projection, ProjectionContext};
use teseladb_mql::regex::Regex;
use teseladb_mql::update::{Now, Update, UpdateContext, upsert};
use teseladb_mql::validation::{ValidationAction, ValidationLevel, Validator, Verdict};
use teseladb_mql::{Filter, FilterUse, MatchContext, MqlError, NoExtensions, codes};

const NOW: Now = Now { millis: 0, timestamp: (0, 0) };

fn update(update: &str, doc: &str) -> Result<String, MqlError> {
    let ext = TestExt::default();
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position: None,
        now: NOW,
        insert: false,
    };
    let u = bytes(&format!(r#"{{"u": {update}}}"#));
    let d = bytes(doc);
    let parsed = Update::parse(raw(&u).get("u").unwrap(), None)?;
    Ok(json(&parsed.apply(raw(&d), &ctx)?.doc))
}

fn dec(s: &str) -> String {
    format!(r#"{{"$numberDecimal": "{s}"}}"#)
}

#[test]
fn decimal_arithmetic() {
    let inc = |a: &str, b: &str| {
        update(&format!(r#"{{"$inc": {{"a": {b}}}}}"#), &format!(r#"{{"a": {a}}}"#)).unwrap()
    };
    let mul = |a: &str, b: &str| {
        update(&format!(r#"{{"$mul": {{"a": {b}}}}}"#), &format!(r#"{{"a": {a}}}"#)).unwrap()
    };
    let is = |got: String, want: &str| assert_eq!(got, canon(&format!(r#"{{"a": {want}}}"#)));
    is(inc(&dec("1"), &dec("-1.5")), &dec("-0.5"));
    is(inc(&dec("-2"), &dec("0.5")), &dec("-1.5"));
    is(inc(&dec("1.5"), &dec("-1.5")), &dec("0.0"));
    is(
        inc(&dec("9999999999999999999999999999999999"), &dec("1")),
        &dec("1.000000000000000000000000000000000E+34"),
    );
    is(
        inc(&dec("1234567890123456789012345678901234"), &dec("0.5")),
        &dec("1234567890123456789012345678901234"),
    );
    is(
        inc(&dec("1234567890123456789012345678901235"), &dec("0.5")),
        &dec("1234567890123456789012345678901236"),
    );
    is(
        inc(&dec("1234567890123456789012345678901234"), &dec("0.51")),
        &dec("1234567890123456789012345678901235"),
    );
    is(
        inc(&dec("1234567890123456789012345678901234"), &dec("0.4")),
        &dec("1234567890123456789012345678901234"),
    );
    is(inc(&dec("1E+6000"), &dec("1E-6000")), &dec("1.000000000000000000000000000000000E+6000"));
    is(inc(&dec("NaN"), "1"), &dec("NaN"));
    is(inc(&dec("Infinity"), &dec("-Infinity")), &dec("NaN"));
    is(inc(&dec("Infinity"), &dec("Infinity")), &dec("Infinity"));
    is(inc(&dec("-Infinity"), "1"), &dec("-Infinity"));
    is(inc(r#"{"$numberDouble": "NaN"}"#, &dec("1")), &dec("NaN"));
    is(inc(r#"{"$numberDouble": "-Infinity"}"#, &dec("1")), &dec("-Infinity"));
    is(inc("0.0", &dec("1")), &dec("1"));
    is(inc("-0.0", &dec("0")), &dec("0"));
    is(inc(r#"{"$numberLong": "-5"}"#, &dec("1")), &dec("-4"));
    is(inc("-5", &dec("1")), &dec("-4"));
    is(mul(&dec("Infinity"), &dec("0")), &dec("NaN"));
    is(mul(&dec("Infinity"), "-2"), &dec("-Infinity"));
    is(mul(&dec("-Infinity"), &dec("-Infinity")), &dec("Infinity"));
    is(mul(&dec("9.999999999999999999999999999999999E+6144"), "10"), &dec("Infinity"));
    is(mul(&dec("1E+6100"), &dec("1E+20")), &dec("1.000000000E+6120"));
    is(mul(&dec("1E-6176"), &dec("0.1")), &dec("0E-6176"));
    is(mul(&dec("6E-6176"), &dec("0.1")), &dec("1E-6176"));
    is(mul(&dec("5E-6176"), &dec("0.1")), &dec("0E-6176"));
    is(mul(&dec("15E-6176"), &dec("0.1")), &dec("2E-6176"));
    is(mul(&dec("123"), &dec("0")), &dec("0"));
    is(mul("2.5", "4"), "10.0");
    is(mul(r#"{"$numberLong": "3"}"#, "2"), r#"{"$numberLong": "6"}"#);
    is(inc("1.5", r#"{"$numberLong": "1"}"#), "2.5");
}

#[test]
fn update_walk_corners() {
    let ok = |u: &str, d: &str, want: &str| {
        assert_eq!(
            update(u, d).unwrap_or_else(|e| panic!("{u} on {d}: {e}")),
            canon(want),
            "{u} on {d}"
        );
    };
    let code = |u: &str, d: &str| update(u, d).expect_err(u).code;
    ok(r#"{"$inc": {"a.1": 1}}"#, r#"{"a": [1, 2]}"#, r#"{"a": [1, 3]}"#);
    ok(r#"{"$unset": {"a.b.c": 1}}"#, r#"{"a": 1}"#, r#"{"a": 1}"#);
    ok(r#"{"$unset": {"a.5.b": 1}}"#, r#"{"a": [1]}"#, r#"{"a": [1]}"#);
    ok(r#"{"$unset": {"a.x.y": 1}}"#, r#"{"a": [1]}"#, r#"{"a": [1]}"#);
    ok(
        r#"{"$pull": {"a.$[].b": 1}}"#,
        r#"{"a": [{"b": [1, 2]}, {}]}"#,
        r#"{"a": [{"b": [2]}, {}]}"#,
    );
    assert_eq!(code(r#"{"$set": {"a.x.$[]": 1}}"#, r#"{"a": [1]}"#), codes::PATH_NOT_VIABLE);
    assert_eq!(code(r#"{"$set": {"a.b.$[]": 1}}"#, r#"{"a": 5}"#), codes::PATH_NOT_VIABLE);
    assert_eq!(code(r#"{"$unset": {"a.b.$[]": 1}}"#, r#"{"a": 5}"#), codes::BAD_VALUE);
    assert_eq!(code(r#"{"$set": {"a.3.$[]": 1}}"#, r#"{"a": [1]}"#), codes::BAD_VALUE);
    // Measured: a name that starts with `$` below the top level is stored; at the top level
    // the update fails while it runs.
    ok(r#"{"$set": {"a.$[x": 1, "a.$$": 2}}"#, r#"{"a": {}}"#, r#"{"a": {"$$": 2, "$[x": 1}}"#);
    assert_eq!(code(r#"{"$inc": {"$y": 1}}"#, "{}"), codes::DOLLAR_PREFIXED_FIELD_NAME);
    assert_eq!(code(r#"{"b": 1, "$x": 1}"#, "{}"), codes::DOLLAR_PREFIXED_FIELD_NAME);
    ok(r#"{"a": {"$x": 1}}"#, r#"{"a": 1}"#, r#"{"a": {"$x": 1}}"#);
    ok(r#"{"$rename": {"a.b": "c"}}"#, r#"{"a": 5}"#, r#"{"a": 5}"#);
    ok(r#"{"$rename": {"a.b.c": "d"}}"#, r#"{"a": {}}"#, r#"{"a": {}}"#);
    ok(
        r#"{"$rename": {"a.b.c": "d"}}"#,
        r#"{"a": {"b": {"c": 1}}}"#,
        r#"{"a": {"b": {}}, "d": 1}"#,
    );
    ok(
        r#"{"$rename": {"x": "a.b.c"}}"#,
        r#"{"x": 1, "a": {"b": {}}}"#,
        r#"{"a": {"b": {"c": 1}}}"#,
    );
    assert_eq!(code(r#"{"$rename": {"x": "a.b"}}"#, r#"{"x": 1, "a": 5}"#), codes::PATH_NOT_VIABLE);
    assert_eq!(code(r#"{"$rename": {"a.b": "a"}}"#, "{}"), codes::BAD_VALUE);
    assert_eq!(code(r#"{"$rename": {"a.0.b": "c"}}"#, r#"{"a": [{"b": 1}]}"#), codes::BAD_VALUE);
    ok(r#"{"$addToSet": {"a": {"b": 1}}}"#, r#"{"a": []}"#, r#"{"a": [{"b": 1}]}"#);
    ok(r#"{"$set": {"a": {"$each": 1}}}"#, "{}", r#"{"a": {"$each": 1}}"#);
    // `$pop` reads its argument as a whole number, with the messages of RefDb 9.0.4.
    for (value, msg) in [
        ("2.5", "Expected an integer: a: 2.5"),
        (r#"{"$numberDouble": "NaN"}"#, "Expected an integer, but found NaN in: a: nan"),
        ("1e20", "Cannot represent as a 64-bit integer: a: 1e+20"),
        (&dec("1.5"), "Cannot represent as a 64-bit integer: a: 1.5"),
        (&dec("1.0"), ""),
        ("-0.0", "$pop expects 1 or -1, found: 0"),
        (r#"{"$numberLong": "7"}"#, "$pop expects 1 or -1, found: 7"),
        (r#""x""#, r#"Expected a number in: a: "x""#),
        ("null", "Expected a number in: a: null"),
    ] {
        let r = update(&format!(r#"{{"$pop": {{"a": {value}}}}}"#), r#"{"a": [1]}"#);
        match msg {
            "" => assert!(r.is_ok(), "{value}"),
            _ => assert_eq!(r.unwrap_err().message, msg, "{value}"),
        }
    }
    let e = update(
        r#"{"$inc": {"a": 1}}"#,
        r#"{"_id": {"$oid": "0102030405060708090a0b0c"}, "a": "x"}"#,
    )
    .unwrap_err();
    assert!(e.message.contains("{_id: ObjectId('0102030405060708090a0b0c')}"), "{}", e.message);
    assert!(format!("{e}").starts_with("TypeMismatch (14): "));
    for (value, shown) in [
        ("1e20", "1e+20"),
        ("1e-5", "1e-05"),
        ("-0.0", "-0.0"),
        (r#"{"$date": "1970-01-01T00:00:00.005Z"}"#, "new Date(5)"),
        (&dec("1.5"), "1.5"),
        ("true", "true"),
        ("null", "null"),
        (r#"{"$undefined": true}"#, "undefined"),
        (r#"[1, "x"]"#, r#"[ 1, "x" ]"#),
        (r#"{"b": {"c": 1}}"#, "{ b: { c: 1 } }"),
        (r#"{"$timestamp": {"t": 1, "i": 2}}"#, "Timestamp(1, 2)"),
    ] {
        // `{$set: <value>}` is refused with the value in the message, and so is a document
        // given to `$inc`.
        let u = match value.starts_with(r#"{"b""#) {
            true => format!(r#"{{"$inc": {{"a": {value}}}}}"#),
            false => format!(r#"{{"$set": {value}}}"#),
        };
        let e = update(&u, r#"{"a": 1}"#).unwrap_err();
        assert!(e.message.contains(shown), "{value}: {}", e.message);
    }
}

#[test]
fn upsert_corners() {
    let ext = TestExt::default();
    let ctx = UpdateContext {
        matcher: MatchContext { collation: None, ext: &ext, vars: None },
        position: None,
        now: NOW,
        insert: true,
    };
    let up = |filter: &str, u: &str| -> Result<String, MqlError> {
        let f = bytes(filter);
        let filter = Filter::parse(raw(&f), FilterUse::Query)?;
        let ub = bytes(&format!(r#"{{"u": {u}}}"#));
        let update = Update::parse(raw(&ub).get("u").unwrap(), None)?;
        upsert(&filter, &update, &ctx, || teseladb_bson::Value::Int32(1)).map(|d| json(&d))
    };
    assert_eq!(
        up(
            r#"{"a": {"$in": [{"$regularExpression": {"pattern": "x", "options": ""}}]}}"#,
            r#"{"$set": {"b": 1}}"#
        )
        .unwrap(),
        canon(r#"{"_id": 1, "b": 1}"#)
    );
    assert_eq!(
        up(r#"{"a.b": 1, "a": 2}"#, r#"{"$set": {"b": 1}}"#).unwrap_err().message,
        "cannot infer query fields to set, both paths 'a.b' and 'a' are matched"
    );
    assert_eq!(
        up(r#"{"a": 1}"#, r#"[{"$set": {"b": 1}}]"#).unwrap(),
        canon(r#"{"_id": 1, "a": 1, "piped": true}"#)
    );
}

#[test]
fn rare_regular_expressions() {
    let ok = |p: &str, s: &str, want: bool| {
        let re = Regex::new(p, "").unwrap_or_else(|e| panic!("{p}: {e}"));
        assert_eq!(re.is_match(s).unwrap(), want, "/{p}/ on {s:?}");
    };
    ok("^\\Z", "\n", true);
    ok("a\\Z", "a", true);
    ok("(?i)(b)\\1", "bc", false);
    ok("^(?:)*$", "", true);
    ok("^(?:a?){3,}$", "a", true);
    ok("^\\X$", "👍\u{200d}x", false);
    ok("^\\X$", "a\u{200d}👍", false);
    ok("(*UCP)[[:print:]]", "\u{2028}", false);
    ok("(*UCP)[[:punct:]]", "+", true);
    ok("(*UCP)[[:punct:]]", "\u{2200}", false);
    ok("(*UCP)[[:graph:]]", "\u{600}", true);
    ok("(*UCP)[[:graph:]]", "\u{61C}", false);
    ok("[[:print:]]", "\u{7}", false);
    ok("\\p{Xuc}", "\u{A0}", true);
    ok("[[:alpha]", "]", false);
    ok("[[:alpha]", ":", true);
    ok("(?-)a", "a", true);
    ok("(?i-i)a", "A", false);
    ok("(?x-x) a", " a", true);
    ok("(a)?(?<=(?(1)a|bb))x", "bbx", true);
    ok("\\Qab", "ab", true);
    ok("[\\Qab\\E]", "b", true);
    ok("a(?#c)+", "aa", true);
    ok("(?x)a # c\n+", "aa", true);
    ok("a{2}{,", "aa{,", true);
    ok("\\g1(a)", "a", false);
    for p in [
        "\\g{-0}",
        "\\g{1",
        "\\o{12",
        "\\x{41",
        "\\p{",
        "\\pé",
        "[\\",
        "[a",
        "[\\Qab",
        "(?<x234567890123456789012345678901234>a)",
        "(?<x>a)(?P>y)",
        "(?-1)",
        "(?+1)",
        "(?(?=a",
        "(?(1)a",
        "(?(+1)a)",
        "(?P=n",
        "(?<=a{65535}b)c",
        "(?<!a",
        "(?i",
        "(?(1x",
        "(?(?!a)b|c|d)",
        "x**",
        "a{2}{3}",
    ] {
        assert!(Regex::new(p, "").is_err(), "{p} compiles");
    }
    // A subject that makes the backtracking stack grow past its limit.
    let re = Regex::new("^(?:a|b)*$", "").unwrap();
    assert!(re.is_match(&format!("{}x", "a".repeat(700_000))).is_err());
}

#[test]
fn filter_corners() {
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let m = |f: &str, d: &str| {
        let (fb, db) = (bytes(f), bytes(d));
        Filter::parse(raw(&fb), FilterUse::Query).unwrap().matches(raw(&db), &ctx).unwrap()
    };
    assert!(!m(r#"{"a..b": 1}"#, r#"{"a": [{"b": 1}]}"#));
    assert!(!m(r#"{"a.0": 5}"#, r#"{"a": [4]}"#));
    assert!(!m(r#"{"a": {"$elemMatch": {"$gt": 1}}}"#, r#"{"a": 5}"#));
    assert!(!m(
        r#"{"a": {"$regularExpression": {"pattern": "x", "options": ""}}}"#,
        r#"{"a": {"$regularExpression": {"pattern": "y", "options": ""}}}"#
    ));
    assert!(m(r#"{"$alwaysTrue": 1.0}"#, "{}"));
    assert!(m(r#"{"a": {"$all": [{"b": 1}]}}"#, r#"{"a": [{"b": 1}]}"#));
    assert!(m(r#"{"a": {"$in": [{"b": 1}]}}"#, r#"{"a": {"b": 1}}"#));
    // The last clause through an array gives the position, with the clauses in RefDb's order
    // (kind, then path), as measured on a `refrouter` 9.0.4.
    let d = bytes(r#"{"a": [0, 1, 2, 3], "b": [2]}"#);
    for (filter, want) in [
        (r#"{"a": 1, "b": 2}"#, 0),
        (r#"{"b": 2, "a": 1}"#, 0),
        (r#"{"a": {"$all": [1, 3]}}"#, 3),
        (r#"{"a": {"$all": [3, 1]}}"#, 1),
        (r#"{"a": {"$gt": 0, "$lt": 3}}"#, 1),
        (r#"{"a": {"$lt": 3, "$gt": 0}}"#, 1),
        (r#"{"$and": [{"a": 1}, {"a": 3}]}"#, 3),
        (r#"{"a": {"$in": [3, 1]}}"#, 1),
    ] {
        let f = bytes(filter);
        let pos = Filter::parse(raw(&f), FilterUse::Query)
            .unwrap()
            .match_position(raw(&d), &ctx)
            .unwrap();
        assert_eq!(pos, Some(Some(want)), "{filter}");
    }
    let f = bytes(r#"{"p": {"$near": [0, 0], "$maxDistance": "x"}}"#);
    assert!(Filter::parse(raw(&f), FilterUse::Query).is_err());
}

#[test]
fn projection_corners() {
    let s = bytes(r#"{"a.$": "x"}"#);
    assert_eq!(Projection::parse(raw(&s)).unwrap_err().code.code, 31308);
    let s = bytes(r#"{"a.$": 0}"#);
    assert_eq!(Projection::parse(raw(&s)).unwrap_err().code.code, 31395);
    let s = bytes(r#"{"a.$.$": 1}"#);
    assert_eq!(Projection::parse(raw(&s)).unwrap_err().code.code, 31394);
    // Not the `find` form of `$slice`: the expression, which 010 evaluates.
    let s = bytes(r#"{"a": {"$slice": ["x", 1]}}"#);
    assert!(Projection::parse(raw(&s)).is_ok());
    let s = bytes(r#"{"a.$": true, "s": {"$meta": "textScore"}}"#);
    let d = bytes(r#"{"a": [7, 8]}"#);
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: Some(1),
        sort_key: None,
        index_key: None,
    };
    assert_eq!(
        json(&Projection::parse(raw(&s)).unwrap().apply(raw(&d), &ctx).unwrap()),
        canon(r#"{"a": [8]}"#)
    );
}

#[test]
fn validation_corners() {
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let warned = |spec: &str, doc: &str| {
        let (s, d) = (bytes(spec), bytes(doc));
        let v = Validator::parse(raw(&s), ValidationLevel::Strict, ValidationAction::Warn).unwrap();
        match v.check(raw(&d), None, false, &ctx).unwrap() {
            Verdict::Warned(info) => json(&info),
            other => panic!("{other:?}"),
        }
    };
    assert!(
        warned(r#"{"a": 1, "$comment": "c", "b": 1}"#, r#"{"a": 2, "b": 1}"#)
            .contains(r#""index":{"$numberInt":"0"}"#)
    );
    assert!(warned(r#"{"a": {"b": 1}}"#, r#"{"a": 2}"#).contains(r#""operatorName":"$eq""#));
    assert!(
        warned(r#"{"a": {"$regex": "x", "$options": "i"}}"#, r#"{"a": "y"}"#)
            .contains(r#""$options":"i""#)
    );
    assert!(
        warned(r#"{"a": {"$options": "i", "$regex": "x"}}"#, r#"{"a": "y"}"#)
            .contains("regular expression did not match")
    );
    let s = bytes(r#"{"a": 1}"#);
    let v = Validator::parse(raw(&s), ValidationLevel::Strict, ValidationAction::Error).unwrap();
    assert_eq!(v.check(raw(&bytes(r#"{"a": 1}"#)), None, false, &ctx).unwrap(), Verdict::Valid);
}

#[test]
fn integration_defaults() {
    let ctx = ProjectionContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: None,
        sort_key: None,
        index_key: None,
    };
    let s = bytes(r#"{"s": {"$meta": "textScore"}}"#);
    let d = bytes(r#"{"a": 1}"#);
    assert_eq!(
        json(&Projection::parse(raw(&s)).unwrap().apply(raw(&d), &ctx).unwrap()),
        canon(r#"{"a": 1}"#)
    );
}
