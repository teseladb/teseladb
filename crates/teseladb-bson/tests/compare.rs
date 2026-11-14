//! The reference server's comparison order: the type classes in order, numbers by
//! exact value across int32, int64, double and decimal128 (NaN equal to itself and first, `-0`
//! equal to `0`), strings by bytes or by a collation, documents and arrays pair by pair with the
//! type class before the name, and the rules of every other type.

use std::cmp::Ordering;

use teseladb_bson::{
    Collator, Decimal128, Document, RawDoc, Value, compare, compare_docs, compare_for_sort, types,
};

/// Compares two values through documents `{v: a}` and `{v: b}`.
fn cmp_with(a: &Value, b: &Value, collation: Option<&dyn Collator>) -> Ordering {
    let (da, db) = (doc(a), doc(b));
    let (ra, rb) = (
        RawDoc::from_bytes(&da).expect("from_bytes succeeds"),
        RawDoc::from_bytes(&db).expect("from_bytes succeeds"),
    );
    compare(ra.get("v").expect("get succeeds"), rb.get("v").expect("get succeeds"), collation)
}

fn cmp(a: &Value, b: &Value) -> Ordering {
    cmp_with(a, b, None)
}

fn doc(v: &Value) -> Vec<u8> {
    Document::new().with("v", v.clone()).to_bytes().expect("to_bytes succeeds")
}

fn dec(s: &str) -> Value {
    Value::Decimal128(s.parse::<Decimal128>().expect("parse succeeds"))
}

/// Every pair of `ordered` compares as their positions; equal neighbours are given in the same
/// inner slice.
fn assert_order(ordered: &[&[Value]]) {
    for (i, group_a) in ordered.iter().enumerate() {
        for (j, group_b) in ordered.iter().enumerate() {
            for a in *group_a {
                for b in *group_b {
                    assert_eq!(cmp(a, b), i.cmp(&j), "{a:?} against {b:?}");
                }
            }
        }
    }
}

#[test]
fn the_type_classes_in_order() {
    let scope = Value::JavaScriptWithScope { code: "f".into(), scope: Document::new() };
    assert_order(&[
        &[Value::MinKey],
        &[Value::Undefined],
        &[Value::Null],
        &[Value::Int32(5), Value::Int64(5), Value::Double(5.0), dec("5")],
        &[Value::String("a".into()), Value::Symbol("a".into())],
        &[Value::Document(Document::new())],
        &[Value::Array(Vec::new())],
        &[Value::Binary { subtype: 0, bytes: Vec::new() }],
        &[Value::ObjectId([0; 12])],
        &[Value::Boolean(false)],
        &[Value::DateTime(0)],
        &[Value::Timestamp { time: 0, increment: 0 }],
        &[Value::Regex { pattern: String::new(), options: String::new() }],
        &[Value::DbPointer { namespace: String::new(), id: [0; 12] }],
        &[Value::JavaScript(String::new())],
        &[scope],
        &[Value::MaxKey],
    ]);
}

#[test]
#[expect(
    clippy::cast_precision_loss,
    reason = "2^53 and its neighbours are the values the comparison is about"
)]
fn numbers_by_exact_value() {
    let p53 = 9_007_199_254_740_992i64;
    assert_order(&[
        &[Value::Double(f64::NAN), dec("NaN"), dec("-NaN")],
        &[Value::Double(f64::NEG_INFINITY), dec("-Infinity")],
        &[Value::Int64(i64::MIN), Value::Double(-(2f64.powi(63))), dec("-9223372036854775808")],
        &[Value::Int32(-1), Value::Double(-1.0), dec("-1.000")],
        &[Value::Double(-0.0), Value::Double(0.0), Value::Int32(0), dec("-0"), dec("0E+5")],
        &[dec("0.1")],
        // The double nearest 0.1 is a little more than 0.1.
        &[Value::Double(0.1)],
        &[Value::Int32(1), Value::Int64(1), Value::Double(1.0), dec("1.0")],
        &[Value::Int64(p53), Value::Double(p53 as f64)],
        // 2^53 + 1 has no double: the int64 is past the double next to it.
        &[Value::Int64(p53 + 1)],
        &[dec("1E+308")],
        // The double nearest 1e308 is a little more than 10^308.
        &[Value::Double(1e308)],
        &[dec("9.999999999999999999999999999999999E+6144")],
        &[Value::Double(f64::INFINITY), dec("Infinity")],
    ]);
    // Tiny numbers: the smallest double against decimals around it.
    // 2^-1074 = 4.9406564584124654417656879286822137…E-324.
    let tiny = f64::from_bits(1);
    assert_eq!(
        cmp(&Value::Double(tiny), &dec("4.940656458412465441765687928682213E-324")),
        Ordering::Greater
    );
    assert_eq!(
        cmp(&Value::Double(tiny), &dec("4.940656458412465441765687928682214E-324")),
        Ordering::Less
    );
    assert_eq!(
        cmp(&Value::Double(-tiny), &dec("-4.940656458412465441765687928682214E-324")),
        Ordering::Greater
    );
    assert_eq!(cmp(&Value::Int64(i64::MAX), &Value::Double(2f64.powi(63))), Ordering::Less);
}

/// Orders strings without case (ASCII), as a collation with strength 2 would.
struct NoCase;

impl Collator for NoCase {
    fn sort_key(&self, s: &str) -> Vec<u8> {
        s.to_ascii_lowercase().into_bytes()
    }
}

#[test]
fn strings_by_bytes_or_by_collation() {
    let s = |x: &str| Value::String(x.into());
    assert_order(&[&[s("")], &[s("B")], &[s("a")], &[s("ab")], &[s("b")], &[s("é")]]);
    assert_eq!(cmp(&s("a"), &Value::Symbol("a".into())), Ordering::Equal);
    assert_eq!(cmp_with(&s("A"), &s("a"), Some(&NoCase)), Ordering::Equal);
    assert_eq!(cmp_with(&s("B"), &s("a"), Some(&NoCase)), Ordering::Greater);
    assert_eq!(cmp_with(&Value::Symbol("B".into()), &s("a"), Some(&NoCase)), Ordering::Greater);
    // The collation reaches strings inside documents and arrays.
    let d = |x: &str| Value::Document(Document::new().with("k", x));
    assert_eq!(cmp_with(&d("X"), &d("x"), Some(&NoCase)), Ordering::Equal);
    assert_eq!(
        cmp_with(&Value::Array(vec![s("X")]), &Value::Array(vec![s("x")]), Some(&NoCase)),
        Ordering::Equal
    );
}

#[test]
fn documents_and_arrays_pair_by_pair() {
    let d = |fields: &[(&str, Value)]| {
        Value::Document(fields.iter().fold(Document::new(), |d, (k, v)| d.with(*k, v.clone())))
    };
    let (one, two, x) = (Value::Int32(1), Value::Int32(2), Value::String("x".into()));
    assert_order(&[
        &[d(&[])],
        &[d(&[("a", one.clone())]), d(&[("a", Value::Double(1.0))])],
        &[d(&[("a", one.clone()), ("b", one.clone())])],
        &[d(&[("a", two.clone())])],
        // The type class goes before the name: a number under "b" before a string under "a".
        &[d(&[("b", one.clone())])],
        &[d(&[("a", x.clone())])],
    ]);
    let a = |items: &[Value]| Value::Array(items.to_vec());
    assert_order(&[
        &[a(&[])],
        &[a(std::slice::from_ref(&one))],
        &[a(&[one.clone(), one.clone()])],
        &[a(&[one.clone(), two.clone()])],
        &[a(&[two])],
    ]);
}

#[test]
fn every_other_type() {
    let bin = |subtype: u8, bytes: &[u8]| Value::Binary { subtype, bytes: bytes.to_vec() };
    // Length, then subtype, then bytes.
    assert_order(&[
        &[bin(types::BINARY_USER, &[9])],
        &[bin(0, &[1, 0])],
        &[bin(0, &[1, 1])],
        &[bin(1, &[0, 0])],
        &[bin(0, &[0, 0, 0])],
    ]);
    // An old binary's stored length counts its inner 4 bytes: 1 byte of subtype 2 ties in length
    // with 5 bytes of subtype 0, which goes first by subtype.
    assert_order(&[&[bin(0, &[9; 5])], &[bin(types::BINARY_OLD, &[0])]]);
    assert_order(&[&[Value::ObjectId([0; 12])], &[Value::ObjectId([1; 12])]]);
    assert_order(&[&[Value::Boolean(false)], &[Value::Boolean(true)]]);
    // Dates are signed; timestamps unsigned, the time first.
    assert_order(&[&[Value::DateTime(i64::MIN)], &[Value::DateTime(-1)], &[Value::DateTime(0)]]);
    let ts = |time: u32, increment: u32| Value::Timestamp { time, increment };
    assert_order(&[&[ts(0, u32::MAX)], &[ts(1, 0)], &[ts(1, 1)], &[ts(u32::MAX, 0)]]);
    let re = |p: &str, o: &str| Value::Regex { pattern: p.into(), options: o.into() };
    assert_order(&[&[re("a", "x")], &[re("ab", "")], &[re("ab", "i")], &[re("b", "")]]);
    // DBPointer: by size, then bytes.
    let ptr = |ns: &str, b: u8| Value::DbPointer { namespace: ns.into(), id: [b; 12] };
    assert_order(&[&[ptr("b", 0)], &[ptr("b", 1)], &[ptr("aa", 0)]]);
    let code = |c: &str| Value::JavaScript(c.into());
    assert_order(&[&[code("a")], &[code("b")]]);
    let scoped = |c: &str, n: i32| Value::JavaScriptWithScope {
        code: c.into(),
        scope: Document::new().with("n", n),
    };
    assert_order(&[&[scoped("a", 2)], &[scoped("b", 1)], &[scoped("b", 2)]]);
    assert_order(&[&[Value::Null], &[Value::MaxKey]]);
    assert_eq!(cmp(&Value::Undefined, &Value::Undefined), Ordering::Equal);
    assert_eq!(cmp(&Value::MinKey, &Value::MinKey), Ordering::Equal);
    assert_eq!(types::MIN_KEY, 0xFF);
}

/// Compares as an in-memory sort does.
fn sort_cmp(a: &Value, b: &Value) -> Ordering {
    let d = Document::new()
        .with("a", a.clone())
        .with("b", b.clone())
        .to_bytes()
        .expect("to_bytes succeeds");
    let raw = RawDoc::from_bytes(&d).expect("from_bytes succeeds");
    compare_for_sort(raw.get("a").expect("get succeeds"), raw.get("b").expect("get succeeds"), None)
}

#[test]
fn a_sort_orders_dbpointers_by_size_id_and_namespace() {
    // Measured on a reference router 9.0.4: `$lt` and an index take the namespace before the
    // ObjectId, a sort without an index takes the ObjectId first.
    let p = |ns: &str, id: u8| Value::DbPointer { namespace: ns.into(), id: [id; 12] };
    let (a, b, c) = (p("db.a", 0xFF), p("db.b", 0), p("db.aa", 0));
    assert_eq!(cmp(&a, &b), Ordering::Less);
    assert_eq!(sort_cmp(&b, &a), Ordering::Less);
    assert_eq!(sort_cmp(&a, &c), Ordering::Less);
    assert_eq!(sort_cmp(&p("db.a", 1), &p("db.b", 1)), Ordering::Less);
    // Inside documents and arrays too; the rest of the order is the same.
    let inside =
        |v: &Value| Value::Document(Document::new().with("x", Value::Array(vec![v.clone()])));
    assert_eq!(sort_cmp(&inside(&b), &inside(&a)), Ordering::Less);
    assert_eq!(cmp(&inside(&a), &inside(&b)), Ordering::Less);
    assert_eq!(sort_cmp(&Value::Int32(1), &Value::Double(0.5)), Ordering::Greater);
    assert_eq!(sort_cmp(&Value::from("a"), &a), Ordering::Less);
    // Whole documents compare in the order of queries.
    let (da, db) = (doc(&a), doc(&b));
    let (ra, rb) = (
        RawDoc::from_bytes(&da).expect("from_bytes succeeds"),
        RawDoc::from_bytes(&db).expect("from_bytes succeeds"),
    );
    assert_eq!(compare_docs(ra, rb, None), Ordering::Less);
}
