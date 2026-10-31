//! Collection validators on inserts and updates (006 T040, FR-022): query operators and
//! `$jsonSchema`, the levels and actions, `bypassDocumentValidation` and the detailed `errInfo`.

pub mod common;

use common::{TestExt, bytes, canon, json, raw};
use teseladb_mql::validation::{ValidationAction, ValidationLevel, Validator, Verdict};
use teseladb_mql::{MatchContext, codes};

fn validator(spec: &str, level: ValidationLevel, action: ValidationAction) -> Validator {
    let b = bytes(spec);
    Validator::parse(raw(&b), level, action).unwrap_or_else(|e| panic!("{spec}: {e}"))
}

fn check(
    v: &Validator,
    doc: &str,
    old: Option<&str>,
    bypass: bool,
) -> Result<Verdict, teseladb_mql::MqlError> {
    let ext = TestExt::default();
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let d = bytes(doc);
    let o = old.map(bytes);
    v.check(raw(&d), o.as_deref().map(raw), bypass, &ctx)
}

#[test]
fn levels_and_actions() {
    let strict =
        validator(r#"{"a": {"$gt": 0}}"#, ValidationLevel::Strict, ValidationAction::Error);
    assert_eq!(check(&strict, r#"{"a": 1}"#, None, false).unwrap(), Verdict::Valid);
    let e = check(&strict, r#"{"_id": 7, "a": 0}"#, None, false).unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (codes::DOCUMENT_VALIDATION_FAILURE, "Document failed validation")
    );
    assert_eq!(
        json(e.info.as_ref().unwrap()),
        canon(
            r#"{"failingDocumentId": 7, "details": {"operatorName": "$gt", "specifiedAs": {"a": {"$gt": 0}}, "reason": "comparison failed", "consideredValue": 0}}"#
        )
    );
    assert_eq!(
        check(&strict, r#"{"a": 0}"#, Some(r#"{"a": 0}"#), false).unwrap_err().code,
        codes::DOCUMENT_VALIDATION_FAILURE
    );
    assert_eq!(check(&strict, r#"{"a": 0}"#, None, true).unwrap(), Verdict::Bypassed);

    let moderate =
        validator(r#"{"a": {"$gt": 0}}"#, ValidationLevel::Moderate, ValidationAction::Error);
    assert_eq!(
        check(&moderate, r#"{"a": 0}"#, Some(r#"{"a": -1}"#), false).unwrap(),
        Verdict::Skipped
    );
    assert!(check(&moderate, r#"{"a": 0}"#, Some(r#"{"a": 1}"#), false).is_err());
    assert!(check(&moderate, r#"{"a": 0}"#, None, false).is_err());

    let off = validator(r#"{"a": {"$gt": 0}}"#, ValidationLevel::Off, ValidationAction::Error);
    assert_eq!(check(&off, r#"{"a": 0}"#, None, false).unwrap(), Verdict::Skipped);

    let warn = validator(r#"{"a": {"$gt": 0}}"#, ValidationLevel::Strict, ValidationAction::Warn);
    match check(&warn, r#"{"a": 0}"#, None, false).unwrap() {
        Verdict::Warned(info) => assert!(json(&info).contains("comparison failed")),
        v => panic!("{v:?}"),
    }

    let log =
        validator(r#"{"a": {"$gt": 0}}"#, ValidationLevel::Strict, ValidationAction::ErrorAndLog);
    assert_eq!(log.action(), ValidationAction::ErrorAndLog);
    assert_eq!(
        check(&log, r#"{"a": 0}"#, None, false).unwrap_err().code,
        codes::DOCUMENT_VALIDATION_FAILURE
    );

    let constraint =
        validator(r#"{"a": {"$gt": 0}}"#, ValidationLevel::Constraint, ValidationAction::Error);
    assert_eq!(constraint.level(), ValidationLevel::Constraint);
    assert!(check(&constraint, r#"{"a": 0}"#, None, true).is_err());
    assert!(check(&constraint, r#"{"a": 0}"#, Some(r#"{"a": -1}"#), false).is_err());
    let b = bytes(r#"{"a": 1}"#);
    assert!(
        Validator::parse(raw(&b), ValidationLevel::Constraint, ValidationAction::Warn).is_err()
    );
}

#[test]
fn names_of_levels_and_actions() {
    for l in [
        ValidationLevel::Off,
        ValidationLevel::Strict,
        ValidationLevel::Moderate,
        ValidationLevel::Constraint,
    ] {
        assert_eq!(ValidationLevel::parse(l.name()).unwrap(), l);
    }
    for a in [ValidationAction::Error, ValidationAction::Warn, ValidationAction::ErrorAndLog] {
        assert_eq!(ValidationAction::parse(a.name()).unwrap(), a);
    }
    let e = ValidationLevel::parse("nope").unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            codes::BAD_VALUE,
            "Enumeration value 'nope' for field 'validationLevel' is not a valid value."
        )
    );
    let e = ValidationAction::parse("nope").unwrap_err();
    assert_eq!(
        e.message,
        "Enumeration value 'nope' for field 'validationAction' is not a valid value."
    );
    assert_eq!(ValidationLevel::default(), ValidationLevel::Strict);
    assert_eq!(ValidationAction::default(), ValidationAction::Error);
}

#[test]
fn validators_refuse_some_operators() {
    for spec in
        [r#"{"$where": "1"}"#, r#"{"$text": {"$search": "x"}}"#, r#"{"p": {"$near": [0, 0]}}"#]
    {
        let b = bytes(spec);
        assert_eq!(
            Validator::parse(raw(&b), ValidationLevel::Strict, ValidationAction::Error)
                .unwrap_err()
                .code,
            codes::BAD_VALUE,
            "{spec}"
        );
    }
}

fn details(spec: &str, doc: &str) -> String {
    let v = validator(spec, ValidationLevel::Strict, ValidationAction::Warn);
    match check(&v, doc, None, false).unwrap() {
        Verdict::Warned(info) => json(&info),
        other => panic!("{spec} on {doc}: {other:?}"),
    }
}

#[test]
fn err_info_details() {
    assert_eq!(
        details(r#"{"$jsonSchema": {"required": ["a"]}}"#, r#"{"_id": 1}"#),
        canon(
            r#"{"failingDocumentId": 1, "details": {"operatorName": "$jsonSchema", "schemaRulesNotSatisfied": [
            {"operatorName": "required", "specifiedAs": {"required": ["a"]}, "missingProperties": ["a"]}]}}"#
        )
    );
    assert_eq!(
        details(r#"{"a": 1, "b": {"$type": "string"}}"#, r#"{"_id": 1, "a": 1, "b": 2}"#),
        canon(
            r#"{"failingDocumentId": 1, "details": {"operatorName": "$and", "clausesNotSatisfied": [{"index": 1, "details":
            {"operatorName": "$type", "specifiedAs": {"b": {"$type": "string"}}, "reason": "type did not match", "consideredValue": 2, "consideredType": "int"}}]}}"#
        )
    );
    assert_eq!(
        details(r#"{"a": 1}"#, r#"{"_id": 1}"#),
        canon(
            r#"{"failingDocumentId": 1, "details": {"operatorName": "$eq", "specifiedAs": {"a": 1}, "reason": "field was missing"}}"#
        )
    );
    assert_eq!(
        details(r#"{"$expr": false}"#, r#"{"_id": 1}"#),
        canon(
            r#"{"failingDocumentId": 1, "details": {"operatorName": "$expr", "specifiedAs": {"$expr": false}, "reason": "expression did not match", "expressionResult": false}}"#
        )
    );
    let has = |spec: &str, doc: &str, part: &str| {
        let text = details(spec, doc);
        assert!(text.contains(part), "{spec} on {doc}: {text} lacks {part}");
    };
    has(
        r#"{"$or": [{"a": 1}, {"b": 1}]}"#,
        r#"{"a": 2, "b": 2}"#,
        r#""operatorName":"$or","clausesNotSatisfied":[{"index":{"$numberInt":"0"}"#,
    );
    has(
        r#"{"$nor": [{"a": 1}]}"#,
        r#"{"a": 1}"#,
        r#""operatorName":"$nor","clausesSatisfied":[{"index":{"$numberInt":"0"}"#,
    );
    has(
        r#"{"$and": [{"a": 1}, {"b": 2}]}"#,
        r#"{"a": 1, "b": 1}"#,
        r#""operatorName":"$and","clausesNotSatisfied":[{"index":{"$numberInt":"1"}"#,
    );
    has(r#"{"a": {"$in": [1]}}"#, r#"{"a": 2}"#, "no matching value found in array");
    has(r#"{"a": {"$nin": [1]}}"#, r#"{"a": 1}"#, "matching value found in array");
    has(r#"{"a": {"$regex": "^x"}}"#, r#"{"a": "y"}"#, "regular expression did not match");
    has(
        r#"{"a": {"$regularExpression": {"pattern": "^x", "options": ""}}}"#,
        r#"{"a": "y"}"#,
        r#""operatorName":"$regex""#,
    );
    has(r#"{"a": {"$exists": true}}"#, r#"{}"#, "path does not exist");
    has(r#"{"a": {"$exists": false}}"#, r#"{"a": 1}"#, r#""reason":"path does exist"}"#);
    has(r#"{"a": {"$size": 2}}"#, r#"{"a": [1]}"#, "array length was not equal to given size");
    has(
        r#"{"a": {"$all": [1, 2]}}"#,
        r#"{"a": [1]}"#,
        "array did not contain all specified values",
    );
    has(
        r#"{"a": {"$elemMatch": {"$gt": 1}}}"#,
        r#"{"a": [1]}"#,
        "array did not satisfy the child predicate",
    );
    has(r#"{"a": {"$mod": [2, 0]}}"#, r#"{"a": 1}"#, "$mod did not evaluate to expected remainder");
    has(r#"{"a": {"$bitsAllSet": 1}}"#, r#"{"a": 2}"#, "bitwise operator failed to match");
    has(r#"{"a": {"$not": {"$gt": 1}}}"#, r#"{"a": 2}"#, "comparison succeeded");
    has(r#"{"$nor": [{"a": 2}]}"#, r#"{"a": 2}"#, "clausesSatisfied");
    has(r#"{"a": {"$gt": 1, "$lt": 2}}"#, r#"{"a": 3}"#, "clausesNotSatisfied");
    has(r#"{"d.x": 5}"#, r#"{"d": [{"x": 1}, {"x": 2}]}"#, "consideredValues");
    has(r#"{"z": {"$ne": null}}"#, r#"{"a": 2}"#, "field was missing");
    has(r#"{"$nor": [{"$jsonSchema": {}}]}"#, r#"{"a": 2}"#, "schema matched");
    has(r#"{"$nor": [{"$expr": true}]}"#, r#"{"a": 2}"#, "expression did match");
    has(r#"{"a": {"$not": {"$exists": true}}}"#, r#"{"a": 2}"#, "path does exist");
    has(
        r#"{"s": {"$not": {"$regularExpression": {"pattern": "^x", "options": ""}}}}"#,
        r#"{"s": "xy"}"#,
        "regular expression did match",
    );
    has(r#"{"a": {"$ne": 1}}"#, r#"{"a": 1}"#, "comparison succeeded");
    has(r#"{"a": {"$gt": 1, "$lt": 5}}"#, r#"{"a": 9}"#, r#""operatorName":"$lt""#);
    has(r#"{"a": {"$gt": 1}}"#, r#"{"b": 9}"#, "field was missing");
    has(r#"{"a.b": {"$gt": 1}}"#, r#"{"a": {"b": 0}}"#, r#""consideredValue":{"$numberInt":"0"}"#);
    has(
        r#"{"$jsonSchema": {"required": ["a"]}, "b": 1}"#,
        r#"{"b": 1}"#,
        "schemaRulesNotSatisfied",
    );
    has(r#"{"$comment": "x", "a": 1}"#, r#"{"a": 2}"#, "comparison failed");
    has(r#"{"$alwaysFalse": 1}"#, r#"{}"#, r#""operatorName":"$alwaysFalse""#);
    has(r#"{"a": {"$geoWithin": {"$box": []}}}"#, r#"{"a": 5}"#, "expression did not match");
}
