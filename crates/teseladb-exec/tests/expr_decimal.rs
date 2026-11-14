//! Decimal `$sqrt`, `$exp`, `$ln`, `$log10`, `$log` and `$pow` (010 T007, FR-005): 34 digits, as a
//! reference router 9.0.4 gave each one on 31-10-2026.

use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_exec::expr::{Expr, Scope};

fn eval(e: &str) -> String {
    let holder =
        extjson::parse(&format!(r#"{{"e":{e}}}"#)).expect("json").to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&holder).expect("a document");
    let expr = Expr::parse(raw.get("e").expect("e")).unwrap_or_else(|err| panic!("{e}: {err:?}"));
    match expr.eval(&Document::new(), &Scope::default()) {
        Ok(Some(Value::Decimal128(d))) => d.to_string(),
        other => panic!("{e}: {other:?}"),
    }
}

fn d(s: &str) -> String {
    format!(r#"{{"$numberDecimal":"{s}"}}"#)
}

/// Three results differ from the reference server's in the last of the 34 digits, where the reference server's decimal
/// library does not round correctly (e^10 is 22026.465794806716516957900645284244366…, the reference server
/// gives …28425; e^100 is 2.6881171418161354484126255515800135873…E+43, the reference server gives …580013;
/// log₁₀ 5 is 0.69897000433601880478626110527550697…, the reference server gives …755069; log₃ 2 is
/// 0.63092975357145743709952711434276085…, the reference server gives …427607): TeselaDB gives the correctly
/// rounded digit (DV-062).
#[test]
fn decimal_functions_give_34_digits_as_the_reference() {
    let cases = [
        (format!(r#"{{"$sqrt":{}}}"#, d("2")), "1.414213562373095048801688724209698"),
        (format!(r#"{{"$sqrt":{}}}"#, d("0.5")), "0.7071067811865475244008443621048490"),
        (format!(r#"{{"$sqrt":{}}}"#, d("1E-20")), "1E-10"),
        (
            format!(r#"{{"$sqrt":{}}}"#, d("123456789.123456789")),
            "11111.11106611111096943055498174930",
        ),
        (format!(r#"{{"$sqrt":{}}}"#, d("4")), "2"),
        (format!(r#"{{"$sqrt":{}}}"#, d("0")), "0"),
        (format!(r#"{{"$sqrt":{}}}"#, d("1E+6000")), "1E+3000"),
        (format!(r#"{{"$exp":{}}}"#, d("1")), "2.718281828459045235360287471352662"),
        (format!(r#"{{"$exp":{}}}"#, d("0.5")), "1.648721270700128146848650787814164"),
        (format!(r#"{{"$exp":{}}}"#, d("-1")), "0.3678794411714423215955237701614609"),
        (format!(r#"{{"$exp":{}}}"#, d("10")), "22026.46579480671651695790064528424"),
        (format!(r#"{{"$exp":{}}}"#, d("100")), "2.688117141816135448412625551580014E+43"),
        (format!(r#"{{"$exp":{}}}"#, d("0")), "1"),
        (format!(r#"{{"$exp":{}}}"#, d("-100")), "3.720075976020835962959695803863118E-44"),
        (format!(r#"{{"$exp":{}}}"#, d("1E+10")), "Infinity"),
        (format!(r#"{{"$ln":{}}}"#, d("2")), "0.6931471805599453094172321214581766"),
        (format!(r#"{{"$ln":{}}}"#, d("10")), "2.302585092994045684017991454684364"),
        (format!(r#"{{"$ln":{}}}"#, d("0.5")), "-0.6931471805599453094172321214581766"),
        (format!(r#"{{"$ln":{}}}"#, d("1E-10")), "-23.02585092994045684017991454684364"),
        (format!(r#"{{"$ln":{}}}"#, d("1")), "0"),
        (format!(r#"{{"$ln":{}}}"#, d("1E+100")), "230.2585092994045684017991454684364"),
        (format!(r#"{{"$log10":{}}}"#, d("2")), "0.3010299956639811952137388947244930"),
        (format!(r#"{{"$log10":{}}}"#, d("1000")), "3"),
        (format!(r#"{{"$log10":{}}}"#, d("0.001")), "-3"),
        (format!(r#"{{"$log10":{}}}"#, d("5")), "0.6989700043360188047862611052755070"),
        (format!(r#"{{"$log":[{},{}]}}"#, d("8"), d("2")), "3.000000000000000000000000000000000"),
        (format!(r#"{{"$log":[{},{}]}}"#, d("100"), d("10")), "2"),
        (format!(r#"{{"$log":[{},{}]}}"#, d("2"), d("3")), "0.6309297535714574370995271143427609"),
        (format!(r#"{{"$log":[{},2]}}"#, d("10")), "3.321928094887362347870319429489390"),
        (format!(r#"{{"$pow":[{},{}]}}"#, d("2"), d("0.5")), "1.414213562373095048801688724209698"),
        (format!(r#"{{"$pow":[{},{}]}}"#, d("2"), d("10")), "1024.000000000000000000000000000000"),
        (
            format!(r#"{{"$pow":[{},{}]}}"#, d("1.1"), d("100")),
            "13780.61233982227018411833717208964",
        ),
        (
            format!(r#"{{"$pow":[{},{}]}}"#, d("10"), d("-3")),
            "0.001000000000000000000000000000000000",
        ),
        (format!(r#"{{"$pow":[{},3]}}"#, d("2")), "8.000000000000000000000000000000000"),
        (
            format!(r#"{{"$pow":[{},{}]}}"#, d("-8"), d("0.3333333333333333333333333333333333")),
            "NaN",
        ),
        (format!(r#"{{"$pow":[{},{}]}}"#, d("3"), d("2.5")), "15.58845726811989564174701707355285"),
        (format!(r#"{{"$pow":[{},{}]}}"#, d("-2"), d("3")), "-8.000000000000000000000000000000000"),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(e, want)| {
            let got = eval(e);
            (got != *want).then(|| format!("{e}: {got} instead of {want}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
