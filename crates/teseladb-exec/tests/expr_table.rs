//! Expressions against what a `refrouter` 9.0.4 gave for each one on 31-10-2026 (010 T007, FR-005).
//! Each file of `tests/expr/` holds one expression per line, a tab, and the result: `=> ` and the
//! document `{x: <result>}` in canonical extended JSON (`{}` when the result is missing), or
//! `!! `, the error code, a tab and the message. Every expression reads `tests/expr/doc.json`.

use teseladb_bson::{Document, RawDoc, extjson};
use teseladb_exec::expr::{Ctx, Expr, Scope};

const DOC: &str = include_str!("expr/doc.json");

fn canonical(d: &Document) -> String {
    let bytes = d.to_bytes().expect("small");
    extjson::to_canonical(RawDoc::from_bytes(&bytes).expect("a document"))
}

/// What TeselaDB gives for expression `e`, written as the measurements are.
fn ours(e: &str) -> String {
    let holder =
        extjson::parse(&format!(r#"{{"e":{e}}}"#)).expect("json").to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&holder).expect("a document");
    let doc = extjson::parse(DOC).expect("the document");
    let r = Expr::parse_in(raw.get("e").expect("e"), &Ctx::checked([]))
        .and_then(|x| x.eval(&doc, &Scope::new(doc.clone(), None)));
    match r {
        Ok(v) => {
            let mut out = Document::new();
            if let Some(v) = v {
                out.push("x", v);
            }
            format!("=> {}", canonical(&out))
        }
        Err(err) => format!("!! {}\t{}", err.code.code, err.message),
    }
}

/// Every line of `table` that TeselaDB does not reproduce.
fn mismatches(table: &str) -> Vec<String> {
    table
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (e, want) = line.split_once('\t').expect("an expression and its result");
            let got = ours(e);
            if got == want {
                return None;
            }
            // The same value written differently (`1.0` or `1`, key spacing) is the same result;
            // the text is compared first because a document with a `$date` key reads back as a
            // date.
            let want = match want.strip_prefix("=> ") {
                Some(json) => format!("=> {}", canonical(&extjson::parse(json).expect("json"))),
                None => want.to_owned(),
            };
            (got != want).then(|| format!("{e}\n   got  {got}\n   want {want}"))
        })
        .collect()
}

fn check(table: &str) {
    let wrong = mismatches(table);
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn array_expressions_match_refdb() {
    check(include_str!("expr/arrays.out"));
}

#[test]
fn set_variable_and_object_expressions_match_refdb() {
    check(include_str!("expr/vars.out"));
}

#[test]
fn type_and_conversion_expressions_match_refdb() {
    check(include_str!("expr/types.out"));
}

#[test]
fn string_expressions_match_refdb() {
    check(include_str!("expr/strings.out"));
}

#[test]
fn bitwise_trigonometric_size_hash_and_vector_expressions_match_refdb() {
    check(include_str!("expr/misc.out"));
}

/// Decimal transcendental functions: TeselaDB gives each value correctly rounded to 34 digits
/// in RefDb's form. A line with a third column (`DV-062 => …`) is one where RefDb's decimal
/// library gives another last digit (its `sin(1)` ends in `…2991`, the correctly rounded value,
/// computed independently, in `…2990`); TeselaDB must give that third column exactly.
#[test]
fn decimal_transcendental_functions_are_correctly_rounded() {
    let mut wrong = Vec::new();
    for line in include_str!("expr/trigdec.out").lines().filter(|l| !l.is_empty()) {
        let (e, rest) = line.split_once('\t').expect("an expression and its result");
        let (refdb, exact) = match rest.split_once("\tDV-062 ") {
            Some((m, x)) => (m, Some(x)),
            None => (rest, None),
        };
        let want = exact.unwrap_or(refdb);
        let got = ours(e);
        if got != want {
            wrong.push(format!("{e}\n   got  {got}\n   want {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn extended_json_expressions_match_refdb() {
    check(include_str!("expr/ejson.out"));
}

#[test]
fn date_expressions_match_refdb() {
    check(include_str!("expr/dates.out"));
}

/// Every name of `contracts/expressions.md` with `[]` as its argument: the operators exist with
/// RefDb's first error or result, and the window functions and accumulators that are no
/// expression are unknown.
/// Date text without a format: every form of timelib's grammar, its errors and warnings, the
/// relative dates, the zone abbreviations and the `timezone` argument (`freedates.out`), and
/// each abbreviation timelib accepts (`abbreviations.out`).
#[test]
fn free_date_text_matches_refdb() {
    check(include_str!("expr/freedates.out"));
    check(include_str!("expr/abbreviations.out"));
}

/// Texts made at random: 2 200 from dates, times, zones and relative parts, and twice 2 500
/// from single words, numbers and separators, and batches of 2 000 mostly of numbers.
#[test]
fn random_free_date_text_matches_refdb() {
    check(include_str!("expr/freerand.out"));
    check(include_str!("expr/freerand2.out"));
    check(include_str!("expr/freerand3.out"));
    check(include_str!("expr/freerand4.out"));
    check(include_str!("expr/freerand5.out"));
}

#[test]
fn every_listed_name_takes_an_empty_argument_as_refdb() {
    check(include_str!("expr/names.out"));
}

#[test]
fn arithmetic_expressions_match_refdb() {
    check(include_str!("expr/arith.out"));
}

/// Edge cases of conversions, Extended JSON and the context operators.
#[test]
fn conversion_and_extended_json_edges_match_refdb() {
    check(include_str!("expr/cover.out"));
}
