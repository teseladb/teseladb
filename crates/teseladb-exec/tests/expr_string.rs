//! String expressions (010 T007, FR-005), each result with the value and type a reference router 9.0.4
//! gave on 31-10-2026 (`$project` with `$type` beside each expression).

use teseladb_bson::{Document, Value, extjson};
use teseladb_exec::expr::{Expr, Scope};

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

fn s(text: &str) -> Option<Value> {
    Some(Value::from(text))
}

#[test]
fn strings_read_as_the_reference_does() {
    let cases: &[(&str, Option<Value>)] = &[
        (r#"{"$concat":["a","b"]}"#, s("ab")),
        (r#"{"$concat":["a",null]}"#, Some(Value::Null)),
        (r#"{"$concat":["a","$zz"]}"#, Some(Value::Null)),
        (r#"{"$concat":[]}"#, s("")),
        (r#"{"$toLower":"AbC"}"#, s("abc")),
        (r#"{"$toLower":null}"#, s("")),
        (r#"{"$toLower":"$zz"}"#, s("")),
        (r#"{"$toLower":12}"#, s("12")),
        (r#"{"$toLower":1.5}"#, s("1.5")),
        (r#"{"$toLower":["A"]}"#, s("a")),
        (r#"{"$toUpper":"aé"}"#, s("Aé")),
        (r#"{"$toLower":"$dt"}"#, s("2026-01-01t00:00:00.000z")),
        (r#"{"$strLenBytes":"aé"}"#, int(3)),
        (r#"{"$strLenCP":"aé"}"#, int(2)),
        (r#"{"$substrBytes":["abcdef",1,3]}"#, s("bcd")),
        (r#"{"$substrBytes":["abc",1,-1]}"#, s("bc")),
        (r#"{"$substrBytes":[null,0,1]}"#, s("")),
        (r#"{"$substrBytes":[12345,1,2]}"#, s("23")),
        (r#"{"$substrCP":["aéb",1,1]}"#, s("é")),
        (r#"{"$substr":["abc",1,5]}"#, s("bc")),
        (r#"{"$indexOfBytes":["abcabc","c"]}"#, int(2)),
        (r#"{"$indexOfBytes":["abcabc","c",3]}"#, int(5)),
        (r#"{"$indexOfBytes":["abc","z"]}"#, int(-1)),
        (r#"{"$indexOfBytes":[null,"a"]}"#, Some(Value::Null)),
        (r#"{"$indexOfCP":["aéb","b"]}"#, int(2)),
        (r#"{"$indexOfBytes":["abc","c",0,2]}"#, int(-1)),
        (r#"{"$indexOfBytes":["abc","",5]}"#, int(-1)),
        (
            r#"{"$split":["a,b,,c",","]}"#,
            Some(Value::Array(["a", "b", "", "c"].map(Value::from).to_vec())),
        ),
        (r#"{"$split":[null,","]}"#, Some(Value::Null)),
        (r#"{"$strcasecmp":["abc","ABC"]}"#, int(0)),
        (r#"{"$strcasecmp":["a","B"]}"#, int(-1)),
        (r#"{"$strcasecmp":[null,""]}"#, int(0)),
        (r#"{"$strcasecmp":[1,"1"]}"#, int(0)),
        (r#"{"$toString":1}"#, s("1")),
        (r#"{"$toString":1.5}"#, s("1.5")),
        (r#"{"$toString":{"$numberLong":"5"}}"#, s("5")),
        (r#"{"$toString":true}"#, s("true")),
        (r#"{"$toString":"$dt"}"#, s("2026-01-01T00:00:00.000Z")),
        (r#"{"$toString":null}"#, Some(Value::Null)),
        (r#"{"$toString":{"$oid":"6ae53d5b0db7b29e3429abaa"}}"#, s("6ae53d5b0db7b29e3429abaa")),
        (r#"{"$toString":{"$numberDecimal":"1.50"}}"#, s("1.50")),
        (r#"{"$toString":2.0}"#, s("2")),
        (r#"{"$toString":1e21}"#, s("1e+21")),
        (r#"{"$toString":0.1}"#, s("0.1")),
        (r#"{"$toString":1e-7}"#, s("1e-07")),
        (r#"{"$toString":1e-5}"#, s("1e-05")),
        (r#"{"$toString":0.0001}"#, s("0.0001")),
        (r#"{"$toString":1e16}"#, s("1e+16")),
        (r#"{"$toString":1e15}"#, s("1000000000000000")),
        (r#"{"$toString":1.5e300}"#, s("1.5e+300")),
        (r#"{"$toString":5e-324}"#, s("5e-324")),
    ];
    for (e, want) in cases {
        assert_eq!(&eval(e), want, "{e}");
    }
}

#[test]
fn strings_refuse_what_the_reference_refuses() {
    let cases: &[(&str, i32, &str)] = &[
        (r#"{"$concat":["a",1]}"#, 16702, "$concat only supports strings, not int"),
        (r#"{"$toLower":true}"#, 16007, "can't convert from BSON type bool to String"),
        (r#"{"$strLenBytes":null}"#, 34473, "$strLenBytes requires a string argument, found: null"),
        (r#"{"$strLenCP":1}"#, 34471, "$strLenCP requires a string argument, found: int"),
        (
            r#"{"$substrBytes":["abc",-1,2]}"#,
            50752,
            "$substrBytes:  starting index must be non-negative (got: -1)",
        ),
        (
            r#"{"$substrBytes":["aé",1,1]}"#,
            28657,
            "$substrBytes:  Invalid range, ending index is in the middle of a UTF-8 character.",
        ),
        (
            r#"{"$substrCP":["abc",0,-1]}"#,
            34454,
            "$substrCP: length must be a nonnegative integer.",
        ),
        (
            r#"{"$indexOfBytes":["abc",null]}"#,
            40092,
            "$indexOfBytes requires a string as the second argument, found: null",
        ),
        (r#"{"$split":["abc",""]}"#, 40087, "$split requires a non-empty separator"),
        (
            r#"{"$split":["abc",1]}"#,
            40086,
            "$split requires an expression that evaluates to a string or regular expression as a second argument, found: int",
        ),
    ];
    for (e, code, msg) in cases {
        assert_eq!(error(e), (*code, (*msg).to_owned()), "{e}");
    }
}
